//! VITA-49 framing for the SmartSDR UDP streams.
//!
//! Every stream the radio sends (DAX IQ, DAX audio, panadapter FFT, waterfall,
//! meters, discovery) and the one stream we send it (DAX TX audio) is a
//! VITA-49 packet: a 28-byte header carrying the stream id, FlexRadio's OUI and
//! a packet class code, followed by the payload.
//!
//! Header fields are big-endian, as the standard says. The payloads are not all
//! alike: DAX audio and the meters are big-endian, DAX **IQ** is little-endian
//! and unscaled (see [`detect_iq`]). Byte order is the first thing to check
//! whenever a stream decodes as noise or as clicks.
//!
//! Layout, from the VITA-49 standard as SmartSDR uses it:
//!
//! ```text
//! byte  0        packet type (bits 7-4), C flag (bit 3), T flag (bit 2)
//! byte  1        TSI (bits 7-6), TSF (bits 5-4), packet count (bits 3-0)
//! bytes 2-3      packet size in 32-bit words, including the header
//! bytes 4-7      stream id
//! bytes 8-11     OUI (0x00001C2D for FlexRadio)
//! bytes 12-15    information class (upper 16) + packet class (lower 16)
//! bytes 16-19    integer timestamp
//! bytes 20-27    fractional timestamp
//! ```

/// FlexRadio's IEEE OUI, in the class-id field of every packet they send.
pub const FLEX_OUI: u32 = 0x0000_1C2D;

/// Header length once stream id, class id and both timestamps are present —
/// which is how SmartSDR sends every stream.
pub const HEADER_LEN: usize = 28;

/// Packet class codes (the lower half of the class-id field).
pub mod class {
    pub const METER: u16 = 0x8002;
    pub const PANADAPTER: u16 = 0x8003;
    pub const WATERFALL: u16 = 0x8004;
    pub const OPUS: u16 = 0x8005;
    pub const DAX_REDUCED_BW: u16 = 0x0123;
    pub const DAX_IQ_24: u16 = 0x02E3;
    pub const DAX_IQ_48: u16 = 0x02E4;
    pub const DAX_IQ_96: u16 = 0x02E5;
    pub const DAX_IQ_192: u16 = 0x02E6;
    pub const DAX_AUDIO: u16 = 0x03E3;
    pub const DISCOVERY: u16 = 0xFFFF;

    /// Whether `code` is one of the DAX IQ classes (one per sample rate).
    pub fn is_dax_iq(code: u16) -> bool {
        matches!(code, DAX_IQ_24 | DAX_IQ_48 | DAX_IQ_96 | DAX_IQ_192)
    }

    /// The DAX IQ class for a sample rate, used when a stream must be
    /// recognised by class alone.
    pub fn dax_iq_for_rate(rate_hz: f64) -> u16 {
        match rate_hz.round() as u32 {
            24_000 => DAX_IQ_24,
            48_000 => DAX_IQ_48,
            96_000 => DAX_IQ_96,
            _ => DAX_IQ_192,
        }
    }
}

/// Packet types (upper nibble of byte 0). We only ever build the two
/// "with stream id" forms; the others are parsed for completeness.
pub mod packet_type {
    pub const IF_DATA: u8 = 0x0;
    pub const IF_DATA_WITH_STREAM: u8 = 0x1;
    pub const EXT_DATA: u8 = 0x2;
    pub const EXT_DATA_WITH_STREAM: u8 = 0x3;
}

/// A parsed packet, borrowing the receive buffer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Packet<'a> {
    pub packet_type: u8,
    /// Sequence counter, 0..15, incremented per packet within a stream. A jump
    /// means the network dropped something.
    pub count: u8,
    pub stream_id: u32,
    pub oui: u32,
    pub info_class: u16,
    pub class_code: u16,
    pub payload: &'a [u8],
}

fn be32(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// Parse a datagram. Returns `None` for anything too short or without the
/// stream id / class id SmartSDR always sends.
pub fn parse(buf: &[u8]) -> Option<Packet<'_>> {
    if buf.len() < HEADER_LEN {
        return None;
    }
    let packet_type = buf[0] >> 4;
    let has_class = buf[0] & 0x08 != 0;
    let has_trailer = buf[0] & 0x04 != 0;
    let has_stream = matches!(
        packet_type,
        packet_type::IF_DATA_WITH_STREAM | packet_type::EXT_DATA_WITH_STREAM
    );
    if !has_class || !has_stream {
        return None;
    }
    let count = buf[1] & 0x0F;
    // `packet size` counts 32-bit words including the header. Trust it over the
    // datagram length only when it fits — a truncated read must not panic.
    let words = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    let total = (words * 4).min(buf.len());
    let end = if has_trailer { total.saturating_sub(4) } else { total };
    let class = be32(buf, 12);
    Some(Packet {
        packet_type,
        count,
        stream_id: be32(buf, 4),
        oui: be32(buf, 8) & 0x00FF_FFFF,
        info_class: (class >> 16) as u16,
        class_code: class as u16,
        payload: buf.get(HEADER_LEN..end.max(HEADER_LEN))?,
    })
}

/// Decode a big-endian `f32` payload, appending to `out`. This is the DAX
/// *audio* format; DAX IQ needs [`decode_iq`].
pub fn decode_f32_be(payload: &[u8], out: &mut Vec<f32>) {
    for c in payload.chunks_exact(4) {
        out.push(f32::from_be_bytes([c[0], c[1], c[2], c[3]]));
    }
}

/// How to read a DAX IQ payload: which byte order, and what divides a sample
/// down to ±1.0.
///
/// Two things about DAX IQ are not what the published documentation suggests,
/// both found by decoding a FLEX-8000 on firmware 4.x:
///
/// * The samples are **little-endian**, unlike every header field around them
///   (and unlike DAX audio). Read big-endian, the significant bytes land at the
///   bottom and each sample turns into a tiny integer — near-silence with
///   occasional sample-wide spikes wherever one more mantissa byte is in use.
///   That reads as an impulse train: clicks, a bright line across the
///   waterfall, and an AGC that pumps.
/// * The values are **not normalised**. They arrive as whole numbers in the
///   thousands, i.e. fixed-point samples that were converted to float without
///   scaling — which is the grain of truth in FlexRadio calling DAX IQ "32-bit
///   fixed point, left justified".
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IqDecode {
    pub little_endian: bool,
    /// Divisor that brings a sample to ±1.0.
    pub scale: f32,
}

/// Full scale for the integer-valued floats a FlexRadio sends.
///
/// The radio does not say what its full scale is, so it is derived from what
/// the hardware can do: a 16-bit converter at 245.76 MHz has roughly a -106
/// dBFS noise floor in a 192 kHz slice. Against 2²³ a quiet 20 m band measures
/// about -72 dBFS — some 34 dB of atmospheric noise above the radio's own
/// floor, which is what an antenna on 20 m actually delivers. Against 2³¹ the
/// same band would land *below* the converter's noise floor, which is
/// impossible. Any residual error is a constant, and the S-meter's
/// `cal_offset_db` is where a measured one belongs.
pub const FLEX_IQ_FULL_SCALE: f32 = 8388608.0;

/// Whether a decoded value is one a receiver could plausibly have produced:
/// finite, and either exactly zero or a normal float. Denormals are the giveaway
/// of a byte order read backwards.
fn plausible(v: f32) -> bool {
    v.is_finite() && (v == 0.0 || v.abs() >= f32::MIN_POSITIVE)
}

/// Work out how to read this payload.
///
/// Both byte orders are tried and the one that yields fewer nonsense values
/// wins; the scale then follows from the magnitudes, so a radio that ever does
/// send normalised floats is handled by the same path. An all-zero payload
/// decodes identically either way, so it yields the default rather than
/// locking in a guess.
pub fn detect_iq(payload: &[u8]) -> IqDecode {
    let (mut be_ok, mut le_ok) = (0usize, 0usize);
    let mut le_max = 0.0f32;
    let mut be_max = 0.0f32;
    for c in payload.chunks_exact(4) {
        let be = f32::from_be_bytes([c[0], c[1], c[2], c[3]]);
        let le = f32::from_le_bytes([c[0], c[1], c[2], c[3]]);
        if plausible(be) {
            be_ok += 1;
            be_max = be_max.max(be.abs());
        }
        if plausible(le) {
            le_ok += 1;
            le_max = le_max.max(le.abs());
        }
    }
    let little_endian = le_ok > be_ok;
    let max = if little_endian { le_max } else { be_max };
    // Samples that stay inside a few units are already normalised; anything
    // larger is the radio's unscaled fixed-point domain.
    let scale = if max > 4.0 { FLEX_IQ_FULL_SCALE } else { 1.0 };
    IqDecode { little_endian, scale }
}

/// Decode a DAX IQ payload into interleaved I,Q floats normalised to ±1.0.
pub fn decode_iq(payload: &[u8], d: IqDecode, out: &mut Vec<f32>) {
    for c in payload.chunks_exact(4) {
        let raw = if d.little_endian {
            f32::from_le_bytes([c[0], c[1], c[2], c[3]])
        } else {
            f32::from_be_bytes([c[0], c[1], c[2], c[3]])
        };
        out.push(raw / d.scale);
    }
}

/// One meter reading out of a meter packet: a 16-bit id and its raw 16-bit
/// value. Scaling depends on the meter's declared unit (see
/// [`crate::meters`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeterSample {
    pub id: u16,
    pub raw: i16,
}

/// Decode a meter packet payload: a sequence of `(u16 id, i16 value)` pairs.
pub fn decode_meters(payload: &[u8]) -> Vec<MeterSample> {
    payload
        .chunks_exact(4)
        .map(|c| MeterSample {
            id: u16::from_be_bytes([c[0], c[1]]),
            raw: i16::from_be_bytes([c[2], c[3]]),
        })
        .collect()
}

/// Build a DAX TX audio packet: stereo 24 kHz float samples, big-endian, under
/// an `IF data with stream id` header carrying the radio's DAX audio class.
///
/// `count` is the 4-bit packet counter; the radio uses it to spot drops, so it
/// must advance by one per packet.
pub fn encode_dax_audio(stream_id: u32, count: u8, stereo: &[f32]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HEADER_LEN + stereo.len() * 4);
    let words = (HEADER_LEN + stereo.len() * 4) / 4;
    // Packet type 1 (IF data with stream id), class id present, no trailer.
    buf.push((packet_type::IF_DATA_WITH_STREAM << 4) | 0x08);
    // TSI = other (0b11), TSF = sample count (0b01), then the packet counter.
    buf.push(0xC0 | 0x10 | (count & 0x0F));
    buf.extend_from_slice(&(words as u16).to_be_bytes());
    buf.extend_from_slice(&stream_id.to_be_bytes());
    buf.extend_from_slice(&FLEX_OUI.to_be_bytes());
    buf.extend_from_slice(&(((class::DAX_AUDIO as u32) | (0x534C << 16)).to_be_bytes()));
    // Timestamps: the radio plays TX audio in arrival order and ignores these.
    buf.extend_from_slice(&0u32.to_be_bytes());
    buf.extend_from_slice(&0u64.to_be_bytes());
    for &s in stereo {
        buf.extend_from_slice(&s.to_be_bytes());
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_tx_audio_packet() {
        let samples = [0.5f32, -0.5, 1.0, -1.0];
        let pkt = encode_dax_audio(0x2000_0001, 7, &samples);
        assert_eq!(pkt.len(), HEADER_LEN + 16);

        let p = parse(&pkt).expect("parsed");
        assert_eq!(p.packet_type, packet_type::IF_DATA_WITH_STREAM);
        assert_eq!(p.stream_id, 0x2000_0001);
        assert_eq!(p.oui, FLEX_OUI);
        assert_eq!(p.class_code, class::DAX_AUDIO);
        assert_eq!(p.count, 7);

        let mut out = Vec::new();
        decode_f32_be(p.payload, &mut out);
        assert_eq!(out, samples);
    }

    #[test]
    fn counter_stays_in_four_bits() {
        let pkt = encode_dax_audio(1, 0x1F, &[]);
        let p = parse(&pkt).expect("parsed");
        assert_eq!(p.count, 0x0F);
    }

    #[test]
    fn parses_an_iq_packet_header() {
        // Hand-built: 512 complex samples is what a 4128-byte DAX IQ packet
        // carries, but the header is what matters here.
        let mut pkt = vec![0u8; HEADER_LEN + 8];
        pkt[0] = (packet_type::IF_DATA_WITH_STREAM << 4) | 0x08;
        pkt[1] = 0x03;
        let words = ((HEADER_LEN + 8) / 4) as u16;
        pkt[2..4].copy_from_slice(&words.to_be_bytes());
        pkt[4..8].copy_from_slice(&0x0400_0001u32.to_be_bytes());
        pkt[8..12].copy_from_slice(&FLEX_OUI.to_be_bytes());
        pkt[12..16].copy_from_slice(&(0x534C_0000 | class::DAX_IQ_192 as u32).to_be_bytes());
        pkt[HEADER_LEN..HEADER_LEN + 4].copy_from_slice(&1.0f32.to_be_bytes());
        pkt[HEADER_LEN + 4..].copy_from_slice(&(-1.0f32).to_be_bytes());

        let p = parse(&pkt).expect("parsed");
        assert!(class::is_dax_iq(p.class_code));
        assert_eq!(p.stream_id, 0x0400_0001);
        let mut iq = Vec::new();
        decode_f32_be(p.payload, &mut iq);
        assert_eq!(iq, vec![1.0, -1.0]);
    }

    #[test]
    fn short_and_malformed_datagrams_are_rejected() {
        assert!(parse(&[0u8; 8]).is_none());
        // Packet type 0 has no stream id — not something SmartSDR sends.
        let mut pkt = vec![0u8; HEADER_LEN + 4];
        pkt[0] = 0x08; // class id present, packet type 0
        assert!(parse(&pkt).is_none());
    }

    #[test]
    fn an_oversized_word_count_cannot_read_past_the_buffer() {
        let mut pkt = encode_dax_audio(1, 0, &[1.0, 2.0]);
        pkt[2..4].copy_from_slice(&9999u16.to_be_bytes());
        let p = parse(&pkt).expect("parsed");
        assert_eq!(p.payload.len(), 8);
    }

    #[test]
    fn reads_the_byte_order_a_flexradio_actually_sends() {
        // Words captured from a FLEX-8000 on firmware 4.x, in wire order. Read
        // the right way round they are ordinary samples; read big-endian they
        // become tiny integers with occasional spikes — the click.
        let wire: Vec<u8> = [
            0x00u8, 0x00, 0x86, 0xC4, // -1072.0
            0x00, 0x00, 0x27, 0xC5, // -2672.0
            0x00, 0x00, 0xBC, 0x44, // 1504.0
            0x00, 0x80, 0xFB, 0xC5, // -8048.0
        ]
        .to_vec();

        let d = detect_iq(&wire);
        assert!(d.little_endian, "byte order read backwards");
        assert_eq!(d.scale, FLEX_IQ_FULL_SCALE, "unscaled samples were taken as normalised");

        let mut out = Vec::new();
        decode_iq(&wire, d, &mut out);
        assert!((out[0] + 1072.0 / FLEX_IQ_FULL_SCALE).abs() < 1e-9, "{:?}", out);
        assert!((out[3] + 8048.0 / FLEX_IQ_FULL_SCALE).abs() < 1e-9, "{:?}", out);

        // The neighbouring samples must stay neighbours: it is the 128-fold
        // jump on the fourth word, read the wrong way, that is audible.
        let peak = out.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
        let median = out[0].abs();
        assert!(peak / median < 10.0, "one sample towers over the others: {out:?}");
    }

    #[test]
    fn normalised_floats_are_left_alone() {
        // A stream that does arrive scaled to ±1.0 must not be divided again.
        let mut wire = Vec::new();
        for v in [0.5f32, -0.25, 1e-3, 0.0] {
            wire.extend_from_slice(&v.to_le_bytes());
        }
        let d = detect_iq(&wire);
        assert!(d.little_endian);
        assert_eq!(d.scale, 1.0);
        let mut out = Vec::new();
        decode_iq(&wire, d, &mut out);
        assert_eq!(out, vec![0.5, -0.25, 1e-3, 0.0]);

        // Big-endian normalised floats are recognised as such too.
        let mut wire = Vec::new();
        for v in [0.5f32, -0.25, 1e-3, 0.125] {
            wire.extend_from_slice(&v.to_be_bytes());
        }
        let d = detect_iq(&wire);
        assert!(!d.little_endian);
        assert_eq!(d.scale, 1.0);
    }

    #[test]
    fn decodes_meter_pairs() {
        let payload = [0x00, 0x01, 0x12, 0x34, 0x00, 0x05, 0xFF, 0xFF];
        assert_eq!(
            decode_meters(&payload),
            vec![
                MeterSample { id: 1, raw: 0x1234 },
                MeterSample { id: 5, raw: -1 },
            ]
        );
    }
}
