//! Ham-band sub-segment data (CW / digimode / phone / beacon ranges), shared so
//! the engine can gate skimmers by segment and the UI can draw the band plan.
//!
//! IARU Region 1 HF ranges, mirrored from the UI band-plan overlay. Frequencies
//! are absolute Hz. Only HF amateur bands are covered; a frequency outside every
//! listed segment returns `None`.

use serde::{Deserialize, Serialize};

/// The operating category of a band sub-segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SegmentKind {
    /// CW / Morse sub-band.
    Cw,
    /// Narrow-band data / digimode sub-band (RTTY, PSK, FT8, …).
    Digi,
    /// SSB / phone sub-band.
    Phone,
    /// Beacon sub-band.
    Beacon,
}

/// One band sub-segment: `[lo, hi)` in Hz with its operating category.
#[derive(Debug, Clone, Copy)]
pub struct Segment {
    pub lo: f64,
    pub hi: f64,
    pub kind: SegmentKind,
}

const fn seg(lo: f64, hi: f64, kind: SegmentKind) -> Segment {
    Segment { lo, hi, kind }
}

const M: f64 = 1_000_000.0;

/// HF CW / digi / phone / beacon segments (IARU Region 1), sorted by frequency.
pub const SEGMENTS: &[Segment] = &[
    // 160m
    seg(1.810 * M, 1.838 * M, SegmentKind::Cw),
    seg(1.838 * M, 1.843 * M, SegmentKind::Digi),
    seg(1.843 * M, 2.000 * M, SegmentKind::Phone),
    // 80m
    seg(3.500 * M, 3.570 * M, SegmentKind::Cw),
    seg(3.570 * M, 3.600 * M, SegmentKind::Digi),
    seg(3.600 * M, 3.800 * M, SegmentKind::Phone),
    // 40m
    seg(7.000 * M, 7.040 * M, SegmentKind::Cw),
    seg(7.040 * M, 7.100 * M, SegmentKind::Digi),
    seg(7.100 * M, 7.200 * M, SegmentKind::Phone),
    // 30m (no phone)
    seg(10.100 * M, 10.130 * M, SegmentKind::Cw),
    seg(10.130 * M, 10.150 * M, SegmentKind::Digi),
    // 20m
    seg(14.000 * M, 14.070 * M, SegmentKind::Cw),
    seg(14.070 * M, 14.099 * M, SegmentKind::Digi),
    seg(14.099 * M, 14.101 * M, SegmentKind::Beacon),
    seg(14.101 * M, 14.350 * M, SegmentKind::Phone),
    // 17m
    seg(18.068 * M, 18.095 * M, SegmentKind::Cw),
    seg(18.095 * M, 18.109 * M, SegmentKind::Digi),
    seg(18.109 * M, 18.111 * M, SegmentKind::Beacon),
    seg(18.111 * M, 18.168 * M, SegmentKind::Phone),
    // 15m
    seg(21.000 * M, 21.070 * M, SegmentKind::Cw),
    seg(21.070 * M, 21.150 * M, SegmentKind::Digi),
    seg(21.150 * M, 21.450 * M, SegmentKind::Phone),
    // 12m
    seg(24.890 * M, 24.915 * M, SegmentKind::Cw),
    seg(24.915 * M, 24.930 * M, SegmentKind::Digi),
    seg(24.930 * M, 24.990 * M, SegmentKind::Phone),
    // 10m
    seg(28.000 * M, 28.070 * M, SegmentKind::Cw),
    seg(28.070 * M, 28.190 * M, SegmentKind::Digi),
    seg(28.190 * M, 28.300 * M, SegmentKind::Beacon),
    seg(28.300 * M, 29.700 * M, SegmentKind::Phone),
];

/// The operating category at `hz`, or `None` outside every listed HF segment.
pub fn segment_kind_at(hz: f64) -> Option<SegmentKind> {
    SEGMENTS.iter().find(|s| hz >= s.lo && hz < s.hi).map(|s| s.kind)
}

/// True if `hz` falls in a CW sub-segment.
pub fn is_cw_segment(hz: f64) -> bool {
    segment_kind_at(hz) == Some(SegmentKind::Cw)
}

/// True if `hz` falls in a digimode sub-segment.
pub fn is_digi_segment(hz: f64) -> bool {
    segment_kind_at(hz) == Some(SegmentKind::Digi)
}

/// FT8 dial frequencies (Hz); each mode occupies ~3 kHz of USB audio above it.
pub const FT8_DIALS: &[f64] = &[
    1_840_000.0,
    3_573_000.0,
    7_074_000.0,
    10_136_000.0,
    14_074_000.0,
    18_100_000.0,
    21_074_000.0,
    24_915_000.0,
    28_074_000.0,
];
/// FT4 dial frequencies (Hz).
pub const FT4_DIALS: &[f64] = &[
    3_575_000.0,
    7_047_500.0,
    10_140_000.0,
    14_080_000.0,
    18_104_000.0,
    21_140_000.0,
    24_919_000.0,
    28_180_000.0,
];
/// JS8Call dial frequencies (Hz); occupies ~3 kHz of USB audio above each.
pub const JS8_DIALS: &[f64] = &[
    1_842_000.0,
    3_578_000.0,
    7_078_000.0,
    10_130_000.0,
    14_078_000.0,
    18_104_000.0,
    21_078_000.0,
    24_922_000.0,
    28_078_000.0,
];
/// WSPR dial frequencies (Hz). The 200 Hz WSPR window sits ~1400–1600 Hz above
/// each dial; slow-CW (QRSS/MEPT) beacons cluster just below it (~1000–1400 Hz).
pub const WSPR_DIALS: &[f64] = &[
    1_836_600.0,
    3_568_600.0,
    7_038_600.0,
    10_138_700.0,
    14_095_600.0,
    18_104_600.0,
    21_094_600.0,
    24_924_600.0,
    28_124_600.0,
];
/// Analog-SSTV calling frequencies (Hz); a picture occupies ~2.7 kHz above each.
pub const SSTV_CALLING: &[f64] =
    &[3_730_000.0, 7_171_000.0, 14_230_000.0, 21_340_000.0, 28_680_000.0];

/// RIFP centre frequencies (Hz): the calling frequency the draft names, plus a
/// spot in each segment below it where a wide channel fits (10 m FM, the 6 m
/// and 2 m all-modes parts). These are centres, not lower edges — the CPFSK
/// signal straddles the dial, ±12.5 kHz.
pub const RIFP_CALLING: &[f64] =
    &[29_600_000.0, 51_250_000.0, 144_700_000.0, crate::RIFP_CALLING_HZ];

/// True where the *automatic* / beacon digital modes live and the PSK/RTTY
/// skimmers must not run — their DSP would only produce garbage from these
/// signals. Covers FT8, FT4 and JS8 (dial → +3 kHz), plus the WSPR window and
/// the QRSS/MEPT beacons just below it (~1000–1700 Hz above the WSPR dial).
pub fn is_auto_digi(hz: f64) -> bool {
    FT8_DIALS.iter().any(|&f| (f - 100.0..=f + 3100.0).contains(&hz))
        || FT4_DIALS.iter().any(|&f| (f - 100.0..=f + 3100.0).contains(&hz))
        || JS8_DIALS.iter().any(|&f| (f - 100.0..=f + 3100.0).contains(&hz))
        || WSPR_DIALS.iter().any(|&f| {
            // Dial reference, plus the QRSS + WSPR beacon window above it.
            (f - 100.0..=f + 400.0).contains(&hz) || (f + 1000.0..=f + 1700.0).contains(&hz)
        })
}

/// The narrow sub-bands where PSK31 activity clusters (IARU Region 1). The PSK
/// skimmer runs only here, not across the whole digi segment.
pub const PSK_RANGES: &[(f64, f64)] = &[
    (1_838_000.0, 1_840_000.0),   // 160m
    (3_580_000.0, 3_583_000.0),   // 80m
    (7_038_000.0, 7_042_000.0),   // 40m
    (10_139_000.0, 10_142_000.0), // 30m
    (14_070_000.0, 14_073_000.0), // 20m (below FT8 @ 14.074)
    (18_097_000.0, 18_100_000.0), // 17m (below FT8 @ 18.100)
    (21_070_000.0, 21_073_000.0), // 15m
    (24_920_000.0, 24_923_000.0), // 12m
    (28_118_000.0, 28_122_000.0), // 10m
];

/// The sub-bands where RTTY activity clusters (IARU Region 1).
pub const RTTY_RANGES: &[(f64, f64)] = &[
    (3_580_000.0, 3_600_000.0),   // 80m
    (7_040_000.0, 7_050_000.0),   // 40m
    (10_140_000.0, 10_150_000.0), // 30m
    (14_083_000.0, 14_099_000.0), // 20m (above FT4 @ 14.080)
    (18_101_000.0, 18_109_000.0), // 17m
    (21_080_000.0, 21_120_000.0), // 15m
    (24_921_000.0, 24_930_000.0), // 12m
    (28_083_000.0, 28_120_000.0), // 10m
];

/// FT8 DXpedition (Fox/Hound) dial frequencies (Hz).
///
/// A separate set from [`FT8_DIALS`] on purpose: a DXpedition running Fox mode
/// fills its whole 3 kHz window with hounds, so it is kept off the ordinary
/// calling frequency rather than swamping it. These are WSJT-X's own defaults,
/// which is what every station chasing the expedition will be tuned to.
pub const FT8_DXPED_DIALS: &[f64] = &[
    1_845_000.0,
    3_567_000.0,
    7_056_000.0,
    10_131_000.0,
    14_090_000.0,
    18_095_000.0,
    21_091_000.0,
    24_911_000.0,
    28_091_000.0,
];

/// FT8 dial frequencies above HF (Hz). 6 m has two: 50.313 is the calling
/// frequency and 50.323 is where the DX goes when it is busy, which during a
/// sporadic-E opening is most of the time.
pub const FT8_VHF_DIALS: &[f64] = &[50_313_000.0, 50_323_000.0, 144_174_000.0, 432_174_000.0];

/// Analog-SSTV frequencies beyond the primary calling ones in [`SSTV_CALLING`].
///
/// The secondaries matter more here than in most modes: a picture takes two
/// minutes, so a single frequency per band is occupied a lot of the time and
/// the convention is simply to move up.
/// The Region 2 secondaries that fall outside the Region 1 allocations this
/// build's band edges model — 80 m 3.845 among them — are deliberately absent:
/// an entry `Band::containing` cannot place is one the picker would never show
/// and the transmit lockout would refuse anyway.
pub const SSTV_SECONDARY: &[f64] = &[3_735_000.0, 7_165_000.0, 14_233_000.0, 28_690_000.0];

/// PSK31 dial frequencies (Hz).
///
/// 40 m carries the region split that the rest of the table does not need:
/// 7.040 is where Region 1 activity sits — the bottom of the digimode segment
/// since the 2009 rebandplan, the older 7.035 having been left in the
/// narrow-band part — and 7.070 is the Region 2 and 3 convention.
pub const PSK_DIALS: &[(f64, &str)] = &[
    (1_838_000.0, ""),
    (3_580_000.0, ""),
    (7_040_000.0, "region 1"),
    (7_070_000.0, "regions 2 and 3"),
    (10_142_000.0, ""),
    (14_070_000.0, ""),
    (18_097_000.0, ""),
    (21_070_000.0, ""),
    (24_920_000.0, ""),
    (28_120_000.0, ""),
];

/// RTTY dial frequencies (Hz) — the DX calling spots, plus the Region 2 slots
/// on 40 m and 80 m where the Region 1 ones fall outside the allocation.
pub const RTTY_DIALS: &[(f64, &str)] = &[
    (3_580_000.0, ""),
    (3_590_000.0, "DX calling"),
    (7_040_000.0, "region 1"),
    (7_080_000.0, "regions 2 and 3"),
    (10_142_000.0, ""),
    (14_080_000.0, ""),
    (14_083_000.0, "DX calling"),
    (18_105_000.0, ""),
    (21_080_000.0, ""),
    (24_925_000.0, ""),
    (28_080_000.0, ""),
];

/// FSQCall dial frequencies (Hz), as the mode's own documentation publishes
/// them. The signal sits in the audio passband above each.
pub const FSQ_DIALS: &[f64] = &[
    1_842_000.0,
    3_588_000.0,
    7_105_000.0,
    10_144_000.0,
    14_105_000.0,
    18_106_000.0,
    21_105_000.0,
    24_925_000.0,
    28_105_000.0,
];

/// One conventional operating frequency for a digital mode.
///
/// "Conventional" rather than "legal": these are the spots the mode's own
/// community has settled on, which is what decides whether anybody hears you.
/// The band edges are a separate matter and are enforced separately.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DigiChannel {
    /// Dial frequency in Hz. Every mode in this table is USB on the dial with
    /// its signal in the audio passband above it.
    pub dial_hz: f64,
    /// What distinguishes it from the band's other entries — "DXpedition",
    /// "region 2", "DX calling". Empty for the plain calling frequency, which
    /// is the first entry in a band.
    pub note: &'static str,
}

impl DigiChannel {
    /// True when this frequency is not in a data sub-segment of the IARU
    /// Region 1 band plan.
    ///
    /// Several genuinely conventional frequencies are: the WSJT-X DXpedition
    /// set is a *global* convention chosen around the Region 2 band plan, and
    /// three of its entries land in Region 1's CW and phone segments.
    ///
    /// Worth showing rather than hiding: an operator in Region 1 should know
    /// before they key that the frequency the DX is working on is one their own
    /// band plan does not put data in.
    ///
    /// The wideband modes are exempt — analog SSTV is a phone emission and
    /// RIFP's CPFSK channel is 25 kHz wide, so both belong in the SSB and
    /// all-mode parts of the band by design and flagging them would be noise.
    pub fn outside_r1_data_segment(&self, mode: crate::Mode) -> bool {
        if wideband_by_design(mode) {
            return false;
        }
        // Above HF there are no segments in the table to check against.
        match segment_kind_at(self.dial_hz) {
            Some(kind) => kind != SegmentKind::Digi,
            None => false,
        }
    }
}

/// True for the modes whose signal is too wide for a narrow-data sub-segment,
/// and which therefore belong in the phone / all-mode parts of the band.
fn wideband_by_design(mode: crate::Mode) -> bool {
    matches!(mode, crate::Mode::Sstv | crate::Mode::Rifp)
}

/// The conventional dial frequencies for `mode`, ascending.
///
/// Empty for a mode with no convention of its own — CW, SSB, and the modes
/// whose operating frequency is a property of what they are pointed at rather
/// than of the mode (RF Paint, RADE).
pub fn digi_channels(mode: crate::Mode) -> Vec<DigiChannel> {
    use crate::Mode;
    let plain = |v: &[f64]| -> Vec<DigiChannel> {
        v.iter().map(|&dial_hz| DigiChannel { dial_hz, note: "" }).collect()
    };
    let noted = |v: &[(f64, &'static str)]| -> Vec<DigiChannel> {
        v.iter().map(|&(dial_hz, note)| DigiChannel { dial_hz, note }).collect()
    };

    let mut v = match mode {
        Mode::Js8 => plain(JS8_DIALS),
        Mode::Ft8 => {
            let mut v = plain(FT8_DIALS);
            v.extend(
                FT8_DXPED_DIALS
                    .iter()
                    .map(|&dial_hz| DigiChannel { dial_hz, note: "DXpedition (Fox/Hound)" }),
            );
            v.extend(plain(FT8_VHF_DIALS));
            v
        }
        Mode::Ft4 => plain(FT4_DIALS),
        Mode::Psk => noted(PSK_DIALS),
        Mode::Rtty => noted(RTTY_DIALS),
        Mode::Fsq => plain(FSQ_DIALS),
        Mode::Sstv => {
            let mut v = plain(SSTV_CALLING);
            v.extend(
                SSTV_SECONDARY.iter().map(|&dial_hz| DigiChannel { dial_hz, note: "secondary" }),
            );
            v
        }
        Mode::Rifp => plain(RIFP_CALLING),
        _ => Vec::new(),
    };
    v.sort_by(|a, b| a.dial_hz.total_cmp(&b.dial_hz));
    v
}

/// The conventional dial frequencies for `mode` that fall inside `band`.
///
/// The caller shows a picker when this returns more than one — a band with a
/// single convention has nothing to choose between.
pub fn digi_channels_in(mode: crate::Mode, band: crate::Band) -> Vec<DigiChannel> {
    digi_channels(mode).into_iter().filter(|c| crate::Band::containing(c.dial_hz) == band).collect()
}

/// True in a PSK31 calling sub-band (and clear of the automatic modes).
pub fn is_psk_segment(hz: f64) -> bool {
    !is_auto_digi(hz) && PSK_RANGES.iter().any(|&(lo, hi)| (lo..=hi).contains(&hz))
}

/// True in an RTTY sub-band (and clear of the automatic modes).
pub fn is_rtty_segment(hz: f64) -> bool {
    !is_auto_digi(hz) && RTTY_RANGES.iter().any(|&(lo, hi)| (lo..=hi).contains(&hz))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification() {
        assert_eq!(segment_kind_at(14_030_000.0), Some(SegmentKind::Cw));
        assert_eq!(segment_kind_at(14_074_000.0), Some(SegmentKind::Digi)); // FT8
        assert_eq!(segment_kind_at(14_200_000.0), Some(SegmentKind::Phone));
        assert!(is_cw_segment(7_020_000.0));
        assert!(!is_cw_segment(7_074_000.0));
        assert!(is_digi_segment(7_074_000.0));
        // Outside any HF ham segment.
        assert_eq!(segment_kind_at(15_000_000.0), None);
        assert!(!is_cw_segment(15_000_000.0));
    }

    #[test]
    fn psk_rtty_subbands() {
        // 20m PSK area, clear of FT8 (14.074).
        assert!(is_psk_segment(14_072_000.0));
        assert!(!is_psk_segment(14_074_000.0)); // FT8
        assert!(!is_psk_segment(14_090_000.0)); // RTTY area, not PSK
        assert!(!is_psk_segment(14_200_000.0)); // phone
        // 20m RTTY area, clear of FT4 (14.080) and WSPR/QRSS (around 14.097).
        assert!(is_rtty_segment(14_090_000.0));
        assert!(!is_rtty_segment(14_081_000.0)); // FT4
        assert!(!is_rtty_segment(14_095_600.0)); // WSPR dial
        assert!(!is_rtty_segment(14_097_000.0)); // WSPR signal window (dial + 1400)
        assert!(!is_rtty_segment(14_096_800.0)); // QRSS / WSPR window (dial + 1200)
        assert!(!is_rtty_segment(14_072_000.0)); // PSK area, not RTTY
    }

    #[test]
    fn wspr_and_qrss_excluded() {
        // The WSPR window (dial + 1400–1600) and the QRSS beacons just below it
        // are excluded from PSK/RTTY skimming on every band.
        for &dial in WSPR_DIALS {
            assert!(is_auto_digi(dial + 1500.0), "WSPR window at {dial}");
            assert!(is_auto_digi(dial + 1200.0), "QRSS window at {dial}");
        }
    }

    /// Every conventional frequency has to be inside an amateur band, and the
    /// Region 1 flag has to agree with the segment table.
    ///
    /// There is deliberately no assertion that the frequencies sit in a data
    /// segment. Several genuinely do not: the WSJT-X DXpedition set and the
    /// FSQCall set are both global conventions built around the Region 2 band
    /// plan, and Region 1 puts CW or phone where they land. Pretending
    /// otherwise would mean either dropping frequencies people really use or
    /// asserting something false. What must hold is that every such frequency
    /// is *flagged*, so the picker can say so instead of recommending it
    /// silently.
    #[test]
    fn every_conventional_frequency_is_in_a_band_and_flagged_honestly() {
        use crate::{Band, Mode};
        for mode in Mode::ALL {
            for c in digi_channels(mode) {
                let band = Band::containing(c.dial_hz);
                assert_ne!(band, Band::Gen, "{mode:?} {} is outside every band", c.dial_hz);
                let flagged = c.outside_r1_data_segment(mode);
                match segment_kind_at(c.dial_hz) {
                    // Above HF there are no sub-segments to check against.
                    None => assert!(!flagged, "{mode:?} {} flagged above HF", c.dial_hz),
                    Some(kind) => assert_eq!(
                        flagged,
                        !wideband_by_design(mode) && kind != SegmentKind::Digi,
                        "{mode:?} {} is in a {kind:?} segment and flagged {flagged}",
                        c.dial_hz
                    ),
                }
            }
        }
    }

    /// Exactly which frequencies Region 1 does not put narrow data on.
    ///
    /// Spelled out rather than counted, so adding a frequency that lands in the
    /// CW or phone part of a Region 1 band shows up here as a decision to make
    /// rather than slipping into the picker unnoticed.
    #[test]
    fn the_region_1_mismatches_are_exactly_these() {
        use crate::{Band, Mode};
        let flagged = |mode: Mode| -> Vec<f64> {
            digi_channels(mode)
                .into_iter()
                .filter(|c| c.outside_r1_data_segment(mode))
                .map(|c| c.dial_hz)
                .collect()
        };
        // The DXpedition set: 1.845 lands in the Region 1 phone segment, 3.567
        // and 24.911 in CW ones.
        assert_eq!(flagged(Mode::Ft8), vec![1_845_000.0, 3_567_000.0, 24_911_000.0]);
        // Two of FSQCall's published frequencies sit above the Region 1 data
        // segment on their band; the rest happen to fall inside one.
        assert_eq!(flagged(Mode::Fsq), vec![7_105_000.0, 14_105_000.0]);
        // The everyday modes are clean.
        for m in [Mode::Ft4, Mode::Psk, Mode::Rtty] {
            assert!(flagged(m).is_empty(), "{m:?}: {:?}", flagged(m));
        }
        // The wideband modes are never flagged: they belong where they are.
        for m in [Mode::Sstv, Mode::Rifp] {
            assert!(flagged(m).is_empty(), "{m:?} must not be flagged");
        }
        // ...and the ordinary FT8 calling frequencies never are either.
        for &f in FT8_DIALS {
            assert!(!DigiChannel { dial_hz: f, note: "" }.outside_r1_data_segment(Mode::Ft8));
        }
        for c in digi_channels_in(Mode::Sstv, Band::M20) {
            assert!(!c.outside_r1_data_segment(Mode::Sstv));
        }
    }

    /// The picker only appears where there is a choice, so the modes that are
    /// meant to offer one have to actually have several in a band, and no mode
    /// may list the same frequency twice.
    #[test]
    fn bands_with_a_choice_have_one_and_the_rest_do_not() {
        use crate::{Band, Mode};
        // FT8 on 20 m: the calling frequency and the DXpedition one.
        let ft8_20 = digi_channels_in(Mode::Ft8, Band::M20);
        assert_eq!(ft8_20.len(), 2, "{ft8_20:?}");
        assert_eq!(ft8_20[0].dial_hz, 14_074_000.0);
        assert!(ft8_20[0].note.is_empty(), "the calling frequency leads and is unannotated");
        assert!(ft8_20[1].note.contains("DXpedition"));
        // 6 m has two FT8 frequencies; 2 m has one, so it shows no picker.
        assert_eq!(digi_channels_in(Mode::Ft8, Band::M6).len(), 2);
        assert_eq!(digi_channels_in(Mode::Ft8, Band::M2).len(), 1);
        // FT4 has a single convention everywhere.
        for b in Band::ALL {
            assert!(digi_channels_in(Mode::Ft4, b).len() <= 1, "FT4 on {b:?}");
        }
        // 40 m PSK carries the region split, which is the case the picker
        // exists for.
        assert_eq!(digi_channels_in(Mode::Psk, Band::M40).len(), 2);
        // SSTV on 20 m: the calling frequency and the one to move up to.
        assert_eq!(digi_channels_in(Mode::Sstv, Band::M20).len(), 2);
        // Modes with no convention of their own offer nothing rather than
        // something invented.
        for m in [Mode::Usb, Mode::Cw, Mode::Rade, Mode::RfPaint, Mode::Hell] {
            assert!(digi_channels(m).is_empty(), "{m:?} should have no channel list");
        }
        // No duplicates, and ascending within every mode.
        for m in Mode::ALL {
            let v = digi_channels(m);
            for w in v.windows(2) {
                assert!(w[0].dial_hz < w[1].dial_hz, "{m:?}: {:?} then {:?}", w[0], w[1]);
            }
        }
    }

    #[test]
    fn segments_sorted_and_non_overlapping() {
        for w in SEGMENTS.windows(2) {
            assert!(w[0].hi <= w[1].lo, "overlap: {:?} then {:?}", w[0], w[1]);
        }
    }
}

#[cfg(test)]
mod js8_tests {
    use super::*;

    #[test]
    fn js8_has_conventional_channels() {
        let ch = digi_channels(crate::Mode::Js8);
        assert_eq!(ch.len(), JS8_DIALS.len());
        assert!(ch.iter().any(|c| c.dial_hz == 14_078_000.0), "20 m JS8 missing");
    }

    #[test]
    fn the_js8_subbands_are_off_limits_to_the_skimmers() {
        // Pre-existing gap: the PSK and RTTY skimmers were running across the
        // JS8 sub-bands, where their DSP can only produce garbage.
        for &dial in JS8_DIALS {
            assert!(is_auto_digi(dial + 1500.0), "{dial} + 1500 Hz should be off limits");
        }
    }
}
