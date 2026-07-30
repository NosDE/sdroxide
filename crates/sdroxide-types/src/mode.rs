use serde::{Deserialize, Serialize};

/// Demodulation / modulation mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Mode {
    Lsb,
    Usb,
    Cw,
    Am,
    Sam,
    Nfm,
    Wfm,
    Digu,
    Digl,
    Dsb,
    Spec,
    /// FT8 digital mode — USB underneath, decoded/encoded by the digi engine.
    Ft8,
    /// FT4 digital mode — USB underneath, decoded/encoded by the digi engine.
    Ft4,
    /// PSK31 keyboard mode — USB underneath, streaming BPSK31 decode/encode.
    Psk,
    /// RTTY keyboard mode — USB underneath, streaming FSK/Baudot decode/encode.
    Rtty,
    /// SSTV image mode — USB underneath, image decode/encode by the digi engine.
    Sstv,
    /// Olivia MFSK keyboard mode — USB underneath, tones/bandwidth chosen in setup.
    Olivia,
    /// THOR (DominoEX-family MFSK+FEC) keyboard mode — submode chosen in setup.
    Thor,
    /// FSQ (Fast Simple QSO) IFK keyboard mode — undirected/directed/image.
    Fsq,
    /// RF Paint (Spectrum Painting) — USB underneath; paints text/images
    /// directly onto the receiver's waterfall. Transmit-only (no decode).
    RfPaint,
    /// FreeDV RADE V1 (Radio Autoencoder) digital voice — USB underneath, a
    /// neural codec over an OFDM waveform occupying ~1000–1900 Hz of audio.
    Rade,
    /// Hellschreiber — USB underneath, a facsimile mode that paints a 7×14 dot
    /// matrix per character straight onto the channel. No sync, no framing, no
    /// decoder: the receiver free-runs and the operator's eye reads the raster.
    ///
    /// Appended last on purpose. `Mode` is postcard-encoded by declaration
    /// index and serde-serialised into stored configs, so a new variant may only
    /// go at the end. Where it *appears* is set by [`Mode::ALL`] instead.
    Hell,
    /// RIFP (Radio Image Framing Protocol, draft-dulaunoy-rifp-00) — a
    /// packetised image mode. Unlike every other digital mode here it is not
    /// USB underneath: the `rifp-cpfsk-4800` profile is continuous-phase FSK
    /// straight on the carrier, ±4 kHz at 4800 baud, so the dial *is* the
    /// signal's centre and the channel is ~25 kHz wide. Appended for the same
    /// reason as [`Mode::Hell`].
    Rifp,
    /// HF weather facsimile (WEFAX / radiofax) — USB underneath, an FM
    /// subcarrier carrying a continuous raster. Receive only: the charts are
    /// broadcast by meteorological services, and an amateur station has nothing
    /// to send back. Appended for the same reason as [`Mode::Hell`].
    Wefax,
    /// JS8 — the keyboard/messaging mode built on FT8's 8-FSK waveform. Slotted
    /// like FT8 but conversational rather than a contest exchange: free text,
    /// directed commands and heartbeats, at one of four speeds chosen in setup.
    /// Appended for the same reason as [`Mode::Hell`].
    Js8,
}

impl Mode {
    /// Every mode, in the order they cycle and appear in the picker — which is
    /// deliberately *not* the enum's declaration order (see [`Mode::Hell`]).
    pub const ALL: [Mode; 25] = [
        Mode::Lsb,
        Mode::Usb,
        Mode::Cw,
        Mode::Am,
        Mode::Sam,
        Mode::Nfm,
        Mode::Wfm,
        Mode::Digu,
        Mode::Digl,
        Mode::Dsb,
        Mode::Spec,
        Mode::Ft8,
        Mode::Ft4,
        Mode::Js8,
        Mode::Psk,
        Mode::Rtty,
        Mode::Sstv,
        Mode::Rifp,
        Mode::Wefax,
        Mode::Olivia,
        Mode::Thor,
        Mode::Fsq,
        Mode::Hell,
        Mode::RfPaint,
        Mode::Rade,
    ];

    /// The digital modes handled by a dedicated decode/encode engine (the
    /// slotted FT8/FT4 modes, the continuous keyboard modes, Hell, SSTV, RIFP,
    /// RF Paint). All are USB underneath except RIFP, which is FSK on the
    /// carrier.
    pub const DIGITAL: [Mode; 14] = [
        Mode::Ft8,
        Mode::Ft4,
        Mode::Js8,
        Mode::Psk,
        Mode::Rtty,
        Mode::Olivia,
        Mode::Thor,
        Mode::Fsq,
        Mode::Hell,
        Mode::Sstv,
        Mode::Rifp,
        Mode::Wefax,
        Mode::RfPaint,
        Mode::Rade,
    ];

    /// True for modes that use a dedicated decode/QSO layer over USB.
    pub fn is_digital(self) -> bool {
        matches!(
            self,
            Mode::Ft8
                | Mode::Ft4
                | Mode::Js8
                | Mode::Psk
                | Mode::Rtty
                | Mode::Sstv
                | Mode::Rifp
                | Mode::Olivia
                | Mode::Thor
                | Mode::Fsq
                | Mode::Hell
                | Mode::RfPaint
                | Mode::Rade
                | Mode::Wefax
        )
    }

    /// True for the modes whose transmit waveform is not single-sideband audio
    /// on the carrier, so the dial is the signal's centre rather than its lower
    /// edge. Only RIFP so far: its CPFSK profile keys the carrier itself.
    pub fn is_carrier_centered(self) -> bool {
        matches!(self, Mode::Rifp)
    }

    /// True for the continuous keyboard text modes (PSK31 / RTTY / Olivia / Thor
    /// / FSQ), as opposed to the slotted FT8/FT4 modes. Drives which decode
    /// engine + panel is used.
    pub fn is_text_modem(self) -> bool {
        matches!(self, Mode::Psk | Mode::Rtty | Mode::Olivia | Mode::Thor | Mode::Fsq)
    }

    /// True for the slotted FT8/FT4 modes, as opposed to the continuous
    /// keyboard modems and the image modes. Drives the decode-list / callsign
    /// overlays that only make sense for a slot-based decoder.
    pub fn is_slotted(self) -> bool {
        matches!(self, Mode::Ft8 | Mode::Ft4 | Mode::Js8)
    }

    /// True for JS8. Forks the digi panel to the conversation UI and uses its
    /// own controller: it is slotted like FT8 but carries a chat rather than a
    /// contest exchange, so the Tx1–Tx6 sequencer has nothing to say about it.
    pub fn is_js8(self) -> bool {
        matches!(self, Mode::Js8)
    }

    /// True for the FSQ mode (adds a directed-message / contacts / image layer
    /// on top of the plain keyboard-modem panel).
    pub fn is_fsq(self) -> bool {
        matches!(self, Mode::Fsq)
    }

    /// True for the SSTV image mode. Forks the digi panel to the image UI and
    /// skips the FT8/text-modem overlays.
    pub fn is_sstv(self) -> bool {
        matches!(self, Mode::Sstv)
    }

    /// True for the RIFP image mode. Shares SSTV's image panel (compose,
    /// transmit, gallery) over a packetised protocol and its own modem.
    pub fn is_rifp(self) -> bool {
        matches!(self, Mode::Rifp)
    }

    /// True for the modes that drive the image panel — a picture compositor on
    /// transmit, a live picture and a gallery on receive.
    pub fn is_image(self) -> bool {
        matches!(self, Mode::Sstv | Mode::Rifp)
    }

    /// True for HF weather fax. Its own panel rather than the image one: there
    /// is nothing to compose and nothing to transmit, and what it needs instead
    /// — line rate, index of cooperation, phasing and slant — has no counterpart
    /// in SSTV.
    pub fn is_wefax(self) -> bool {
        matches!(self, Mode::Wefax)
    }

    /// True for the receive-only modes, so the UI can leave the transmit
    /// controls out rather than showing ones that refuse.
    pub fn is_rx_only(self) -> bool {
        matches!(self, Mode::Wefax)
    }

    /// True for Hellschreiber. Forks the digi panel to the scrolling raster UI:
    /// unlike the keyboard modems there is nothing to decode into text, so it
    /// gets its own controller and panel rather than joining `is_text_modem`.
    pub fn is_hell(self) -> bool {
        matches!(self, Mode::Hell)
    }

    /// True for the RF Paint (Spectrum Painting) mode. Forks the digi panel to
    /// the text/image painting UI and uses its own transmit-only controller.
    pub fn is_rf_paint(self) -> bool {
        matches!(self, Mode::RfPaint)
    }

    /// True for FreeDV RADE V1 digital voice. Unlike the other digital modes it
    /// carries speech rather than text or images, so it both replaces the
    /// receive audio and consumes the microphone on transmit.
    pub fn is_rade(self) -> bool {
        matches!(self, Mode::Rade)
    }

    /// Whether the voice keyer may transmit in this mode.
    ///
    /// The digital modes synthesise their own transmit audio, so a recorded
    /// message has nowhere to go — RADE excepted: it carries speech, and takes
    /// the playback as its microphone input exactly like a live over.
    pub fn allows_voice_keyer(self) -> bool {
        !self.is_digital() || self.is_rade()
    }

    pub fn label(self) -> &'static str {
        match self {
            Mode::Lsb => "LSB",
            Mode::Usb => "USB",
            Mode::Cw => "CW",
            Mode::Am => "AM",
            Mode::Sam => "SAM",
            Mode::Nfm => "NFM",
            Mode::Wfm => "WFM",
            Mode::Digu => "DIGU",
            Mode::Digl => "DIGL",
            Mode::Dsb => "DSB",
            Mode::Spec => "SPEC",
            Mode::Ft8 => "FT8",
            Mode::Ft4 => "FT4",
            Mode::Psk => "PSK",
            Mode::Rtty => "RTTY",
            Mode::Sstv => "SSTV",
            Mode::Olivia => "OLIVIA",
            Mode::Thor => "THOR",
            Mode::Fsq => "FSQ",
            Mode::Hell => "HELL",
            Mode::RfPaint => "RFPAINT",
            Mode::Rade => "RADE",
            Mode::Rifp => "RIFP",
            Mode::Wefax => "WEFAX",
            Mode::Js8 => "JS8",
        }
    }

    /// Default audio passband edges in Hz relative to the carrier/VFO.
    /// Negative frequencies are below the carrier (LSB side).
    pub fn default_filter(self) -> (f32, f32) {
        match self {
            Mode::Lsb => (-2850.0, -150.0),
            Mode::Usb => (150.0, 2850.0),
            // CW passband is centered on the sidetone pitch (default 700 Hz).
            Mode::Cw => (450.0, 950.0),
            Mode::Am | Mode::Sam => (-5000.0, 5000.0),
            Mode::Nfm => (-8000.0, 8000.0),
            Mode::Wfm => (-96_000.0, 96_000.0),
            Mode::Digu => (200.0, 3200.0),
            Mode::Digl => (-3200.0, -200.0),
            Mode::Dsb => (-2850.0, 2850.0),
            Mode::Spec => (-5000.0, 5000.0),
            // FT8/FT4 occupy the whole USB audio passband (tones 0..~3500 Hz).
            // PSK/RTTY/Olivia/Thor/FSQ/Hell do the same (the modem filters
            // narrowly around audio_hz — and Hell X9 needs nearly all of it).
            // SSTV occupies the full USB audio passband.
            Mode::Ft8
            | Mode::Ft4
            | Mode::Js8
            | Mode::Psk
            | Mode::Rtty
            | Mode::Sstv
            | Mode::Olivia
            | Mode::Thor
            | Mode::Fsq
            | Mode::Hell
            | Mode::RfPaint => (100.0, 3300.0),
            // The fax subcarrier is 1900 Hz ± 400; the wider passband leaves
            // room for a receiver tuned a few hundred hertz off, which is the
            // normal state of affairs on a chart found by ear.
            Mode::Wefax => (500.0, 3300.0),
            // RIFP is not a sideband mode: the CPFSK carrier sits *on* the
            // dial and swings ±4 kHz, so the passband straddles it. 25 kHz is
            // the profile's recommended occupied bandwidth.
            Mode::Rifp => (-12_500.0, 12_500.0),
            // RADE V1's OFDM carriers sit between roughly 1060 and 1880 Hz;
            // the wider passband leaves room for the acquisition search to
            // track a signal that is off frequency.
            Mode::Rade => (300.0, 2700.0),
        }
    }

    /// True for modes that place the displayed carrier below the passband.
    pub fn is_lower_sideband(self) -> bool {
        matches!(self, Mode::Lsb | Mode::Digl)
    }

    /// Furthest a filter edge may be dragged from the carrier — bounded by
    /// the mode's DSP channel bandwidth.
    pub fn max_filter_hz(self) -> f32 {
        match self {
            Mode::Wfm => 120_000.0,
            _ => 24_000.0,
        }
    }

    /// Filter width presets: (label, lo, hi) relative to the carrier.
    pub fn filter_presets(self) -> &'static [(&'static str, f32, f32)] {
        match self {
            Mode::Usb | Mode::Digu => &[
                ("1.8k", 200.0, 2000.0),
                ("2.4k", 200.0, 2600.0),
                ("2.7k", 150.0, 2850.0),
                ("3.3k", 100.0, 3400.0),
            ],
            Mode::Lsb | Mode::Digl => &[
                ("1.8k", -2000.0, -200.0),
                ("2.4k", -2600.0, -200.0),
                ("2.7k", -2850.0, -150.0),
                ("3.3k", -3400.0, -100.0),
            ],
            Mode::Cw => &[
                ("100", 650.0, 750.0),
                ("250", 575.0, 825.0),
                ("500", 450.0, 950.0),
                ("1k", 200.0, 1200.0),
            ],
            Mode::Am | Mode::Sam => {
                &[("6k", -3000.0, 3000.0), ("10k", -5000.0, 5000.0), ("16k", -8000.0, 8000.0)]
            }
            Mode::Nfm => &[("8k", -4000.0, 4000.0), ("16k", -8000.0, 8000.0)],
            Mode::Dsb => &[("5k", -2500.0, 2500.0), ("6k", -3000.0, 3000.0)],
            // Digital modes have a fixed wide passband; no presets.
            Mode::Wfm
            | Mode::Spec
            | Mode::Ft8
            | Mode::Ft4
            | Mode::Js8
            | Mode::Psk
            | Mode::Rtty
            | Mode::Sstv
            | Mode::Olivia
            | Mode::Thor
            | Mode::Fsq
            | Mode::Hell
            | Mode::RfPaint
            | Mode::Rifp
            | Mode::Wefax
            | Mode::Rade => &[],
        }
    }
}

impl std::str::FromStr for Mode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Mode::ALL
            .into_iter()
            .find(|m| m.label().eq_ignore_ascii_case(s))
            .ok_or_else(|| format!("unknown mode {s:?} (try USB, LSB, CW, AM, SAM, NFM, WFM…)"))
    }
}

/// Audio noise-reduction setting for the demodulated audio. Two engines are
/// offered at three intensities each: a neural **RNNoise** denoiser (`Ai*`) and
/// the classic spectral NR (`Low`/`Medium`/`High`). The button cycles
/// Off → AI Low → AI Med → AI High → NR Low → NR Mid → NR High → Off.
///
/// The spectral variants keep their original discriminant order so persisted
/// configs and the wire protocol stay compatible; the neural variants are
/// appended, and [`NrLevel::next`] imposes the display cycle order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum NrLevel {
    #[default]
    Off,
    Low,
    Medium,
    High,
    AiLow,
    AiMed,
    AiHigh,
}

impl NrLevel {
    pub const ALL: [NrLevel; 7] = [
        NrLevel::Off,
        NrLevel::AiLow,
        NrLevel::AiMed,
        NrLevel::AiHigh,
        NrLevel::Low,
        NrLevel::Medium,
        NrLevel::High,
    ];

    /// Suffix shown after "NR" on the toggle chip (Off shows just "NR").
    pub fn label(self) -> &'static str {
        match self {
            NrLevel::Off => "Off",
            NrLevel::AiLow => "AI Low",
            NrLevel::AiMed => "AI Med",
            NrLevel::AiHigh => "AI High",
            NrLevel::Low => "Low",
            NrLevel::Medium => "Mid",
            NrLevel::High => "High",
        }
    }

    pub fn is_on(self) -> bool {
        !matches!(self, NrLevel::Off)
    }

    /// True for the neural (RNNoise) intensities.
    pub fn is_ai(self) -> bool {
        matches!(self, NrLevel::AiLow | NrLevel::AiMed | NrLevel::AiHigh)
    }

    /// Cycle to the next setting: Off → AI Low/Med/High → NR Low/Mid/High → Off.
    pub fn next(self) -> NrLevel {
        match self {
            NrLevel::Off => NrLevel::AiLow,
            NrLevel::AiLow => NrLevel::AiMed,
            NrLevel::AiMed => NrLevel::AiHigh,
            NrLevel::AiHigh => NrLevel::Low,
            NrLevel::Low => NrLevel::Medium,
            NrLevel::Medium => NrLevel::High,
            NrLevel::High => NrLevel::Off,
        }
    }

    /// Spectral-NR tuning: `(noise over-estimation factor, minimum gain floor)`.
    /// A larger over-estimate removes more of the noise; a lower floor lets weak
    /// bins be attenuated further — more aggressive, at more risk of artefacts.
    /// The over-factors are modest because the MCRA estimator is unbiased (it
    /// tracks the noise mean, not an under-estimated minimum), so ~1.0 already
    /// removes stationary noise; higher values are pure over-subtraction.
    /// Neutral (unused) for Off and the neural variants.
    pub fn params(self) -> (f32, f32) {
        match self {
            NrLevel::Low => (1.0, 0.30),
            NrLevel::Medium => (1.4, 0.14),
            NrLevel::High => (2.0, 0.07),
            _ => (1.0, 1.0),
        }
    }

    /// Neural-NR wet/dry depth (0 = bypass, 1 = full RNNoise). Only meaningful
    /// for the `Ai*` variants.
    pub fn ai_mix(self) -> f32 {
        match self {
            NrLevel::AiLow => 0.55,
            NrLevel::AiMed => 0.8,
            NrLevel::AiHigh => 1.0,
            _ => 0.0,
        }
    }

    /// Make-up gain applied to the listener audio after noise reduction:
    /// suppression lowers the overall level (more so at higher settings), so a
    /// progressively larger boost keeps the perceived loudness roughly constant.
    /// RNNoise preserves speech level far better than spectral subtraction, so
    /// its make-up is gentle.
    pub fn makeup_gain(self) -> f32 {
        match self {
            NrLevel::Off => 1.0,
            NrLevel::AiLow => 1.0,
            NrLevel::AiMed => 1.1,
            NrLevel::AiHigh => 1.2,
            NrLevel::Low => 1.3,
            NrLevel::Medium => 1.7,
            NrLevel::High => 2.1,
        }
    }
}

/// AGC behavior for a receiver channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgcMode {
    Off,
    Slow,
    Med,
    Fast,
}

impl AgcMode {
    pub const ALL: [AgcMode; 4] = [AgcMode::Off, AgcMode::Slow, AgcMode::Med, AgcMode::Fast];

    pub fn label(self) -> &'static str {
        match self {
            AgcMode::Off => "Off",
            AgcMode::Slow => "Slow",
            AgcMode::Med => "Med",
            AgcMode::Fast => "Fast",
        }
    }

    /// Hang time in milliseconds; `None` means AGC disabled.
    pub fn hang_ms(self) -> Option<f32> {
        match self {
            AgcMode::Off => None,
            AgcMode::Slow => Some(1000.0),
            AgcMode::Med => Some(500.0),
            AgcMode::Fast => Some(100.0),
        }
    }
}
