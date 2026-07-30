//! The radio's own spectrum scope, over CI-V.
//!
//! An Icom sends no IQ, so a wideband waterfall cannot be computed from what we
//! receive — but the radio draws one itself and will hand over the finished
//! sweep. That is what wfview shows, and it is the only way to see more than
//! the 3 kHz of audio.
//!
//! Every sweep carries its own centre frequency and span, so turning the SPAN
//! knob on the radio moves our display with it — no setting on this side has to
//! agree with anything on that side.
//!
//! Layout of a `27 00` sweep, counted from the sub-command byte:
//!
//! ```text
//! [0]      00        the sub-command
//! [1]      receiver  00 = main
//! [2] [3]  segment   which part of the sweep, and how many; over the network
//!                    the radio sends it whole (over USB it splits it in eleven)
//! [4]      mode      0 = centre, 1 = fixed, 2 = scroll-C, 3 = scroll-F
//! [5..10]  start     centre mode: the centre frequency; fixed: the lower edge
//! [10..15] end       centre mode: HALF the span; fixed: the upper edge
//! [15]     range     1 = the scope has nothing to show; the points are stale
//! [16..]   points    the waveform, one byte each — about 478 of them
//! ```
//!
//! Verified against hardware in an IC-705/IC-9700 project of the operator's,
//! which had already found the two traps here: the receiver byte after the
//! sub-command is easy to miss (everything then shifts by one and still looks
//! plausible in centre mode), and in centre mode the second frequency is *half*
//! the span, not the end of it.

use sdroxide_cat::civ;

/// Where the waveform starts, counted from the sub-command byte.
const POINTS_AT: usize = 16;
/// Fewest points that count as a sweep rather than a stray reply.
const MIN_POINTS: usize = 64;
/// What a point's value means in decibels.
///
/// The radio's scale is its own: the reference guide allows 0..160, while a
/// real stream sits mostly between 5 (noise) and 100 (a strong signal).
/// Shifting it so the noise lands near -110 dB puts a normal band inside
/// sdroxide's default display window instead of below its floor; the dB
/// sliders and FIT adjust from there.
const DB_OFFSET: f32 = -120.0;

/// Spans the centre-mode scope offers, in Hz — the ± value, so the displayed
/// width is twice this.
pub const SPANS_HZ: [f64; 8] =
    [2_500.0, 5_000.0, 10_000.0, 25_000.0, 50_000.0, 100_000.0, 250_000.0, 500_000.0];

/// One finished sweep.
#[derive(Debug, Clone, PartialEq)]
pub struct Sweep {
    /// Middle of the displayed range.
    pub center_hz: f64,
    /// Width of the displayed range.
    pub span_hz: f64,
    /// One value per point, in dB relative to full scale.
    pub bins_db: Vec<f32>,
}

/// Turn the scope on and start the data flowing. Both are needed: the radio
/// only sends sweeps when its scope is running *and* output is enabled. Output
/// is enabled first, which is the order that has been seen to work.
pub fn enable_frames(radio: u8) -> [Vec<u8>; 2] {
    [civ::frame(radio, 0x27, &[0x11, 0x01]), civ::frame(radio, 0x27, &[0x10, 0x01])]
}

/// Stop the sweeps. Sent on the way out so the radio isn't left streaming to
/// nobody.
pub fn disable_frame(radio: u8) -> Vec<u8> {
    civ::frame(radio, 0x27, &[0x11, 0x00])
}

/// Set the centre-mode span. `span_hz` is the ± value; the nearest one the
/// radio offers is used.
pub fn set_span_frame(radio: u8, span_hz: f64) -> Vec<u8> {
    let span = SPANS_HZ
        .iter()
        .copied()
        .min_by(|a, b| {
            (a - span_hz).abs().partial_cmp(&(b - span_hz).abs()).unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(50_000.0);
    let mut data = vec![0x15];
    data.extend_from_slice(&civ::encode_freq(span));
    civ::frame(radio, 0x27, &data)
}

/// Parse the data of a `27` reply into a sweep.
///
/// `data` is what follows the command byte, so it starts with the sub-command.
/// Anything that is not a whole waveform yields `None`: another sub-command, a
/// sweep the radio split into segments (only the USB port does that), or one it
/// marked out of range.
pub fn parse_sweep(data: &[u8]) -> Option<Sweep> {
    if data.first() != Some(&0x00) || data.len() < POINTS_AT + MIN_POINTS {
        return None;
    }
    let (seg_no, seg_total) = (data[2], data[3]);
    // A segmented sweep would have to be reassembled; over the network the
    // radio never sends one, so there is nothing to reassemble.
    if seg_no != 1 || seg_total != 1 {
        return None;
    }
    let start = civ::decode_freq(data.get(5..10)?)?;
    let end = civ::decode_freq(data.get(10..15)?)?;
    // Out of range: the header still says where the scope is looking, but the
    // points are stale, so the display should freeze rather than draw noise.
    if data[15] == 0x01 {
        return None;
    }

    // Scroll-F behaves like the fixed mode, scroll-C like the centre mode.
    let fixed = matches!(data[4], 1 | 3);
    let (center_hz, span_hz) = if fixed {
        if end <= start {
            return None;
        }
        ((start + end) / 2.0, end - start)
    } else {
        // The second frequency is half the span, not the upper edge.
        (start, end * 2.0)
    };
    if span_hz <= 0.0 || center_hz <= 0.0 {
        return None;
    }

    let bins_db = data[POINTS_AT..].iter().map(|&v| v as f32 + DB_OFFSET).collect();
    Some(Sweep { center_hz, span_hz, bins_db })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sweep in the shape the radio really sends it.
    fn sweep(mode: u8, start: f64, end: f64, level: u8, points: usize) -> Vec<u8> {
        let mut d = vec![0x00, 0x00, 0x01, 0x01, mode];
        d.extend_from_slice(&civ::encode_freq(start));
        d.extend_from_slice(&civ::encode_freq(end));
        d.push(0x00); // in range
        d.extend(std::iter::repeat_n(level, points));
        d
    }

    #[test]
    fn centre_mode_reads_the_second_frequency_as_half_the_span() {
        // The trap: 25 kHz here means a 50 kHz wide display.
        let s = parse_sweep(&sweep(0, 14_100_000.0, 25_000.0, 80, 478)).expect("sweep");
        assert_eq!(s.center_hz, 14_100_000.0);
        assert_eq!(s.span_hz, 50_000.0);
        assert_eq!(s.bins_db.len(), 478, "the point count is whatever the radio sends");
    }

    #[test]
    fn fixed_mode_reads_the_two_edges() {
        let s = parse_sweep(&sweep(1, 14_000_000.0, 14_350_000.0, 40, 478)).expect("sweep");
        assert_eq!(s.center_hz, 14_175_000.0);
        assert_eq!(s.span_hz, 350_000.0);
        // Scroll-F is the fixed mode under another name.
        let scroll = parse_sweep(&sweep(3, 14_000_000.0, 14_350_000.0, 40, 478)).expect("sweep");
        assert_eq!(scroll.span_hz, 350_000.0);
    }

    #[test]
    fn levels_land_where_the_display_can_see_them() {
        // A real stream's noise sits around 5..20 and signals from 30 up.
        let s = parse_sweep(&sweep(0, 14_100_000.0, 25_000.0, 10, 478)).expect("sweep");
        let noise = s.bins_db[0];
        assert!((-115.0..=-105.0).contains(&noise), "noise landed at {noise} dB");
        let s = parse_sweep(&sweep(0, 14_100_000.0, 25_000.0, 90, 478)).expect("sweep");
        assert!(s.bins_db[0] > -40.0, "a strong signal landed at {} dB", s.bins_db[0]);
    }

    #[test]
    fn refuses_what_is_not_a_usable_sweep() {
        // Another sub-command entirely.
        assert!(parse_sweep(&[0x15, 0x00, 0x25, 0x00, 0x00]).is_none());
        // Too short to hold a waveform.
        assert!(parse_sweep(&sweep(0, 14_100_000.0, 25_000.0, 80, 8)).is_none());
        // Out of range: the points are stale, so the display keeps the old ones.
        let mut oor = sweep(0, 14_100_000.0, 25_000.0, 80, 478);
        oor[15] = 0x01;
        assert!(parse_sweep(&oor).is_none());
        // One segment of a split sweep, which only the USB port produces.
        let mut split = sweep(0, 14_100_000.0, 25_000.0, 80, 478);
        split[2] = 0x02;
        split[3] = 0x0B;
        assert!(parse_sweep(&split).is_none());
    }

    #[test]
    fn span_is_snapped_to_what_the_radio_offers() {
        // 30 kHz is not on the list; 25 kHz is the nearest.
        assert_eq!(set_span_frame(0xA4, 30_000.0), set_span_frame(0xA4, 25_000.0));
        let f = set_span_frame(0xA4, 25_000.0);
        assert_eq!(f[4], 0x27);
        assert_eq!(f[5], 0x15);
    }

    #[test]
    fn enabling_takes_both_switches() {
        let [data_on, scope_on] = enable_frames(0xA4);
        assert_eq!(&data_on[4..7], &[0x27, 0x11, 0x01]);
        assert_eq!(&scope_on[4..7], &[0x27, 0x10, 0x01]);
        assert_eq!(&disable_frame(0xA4)[4..7], &[0x27, 0x11, 0x00]);
    }
}
