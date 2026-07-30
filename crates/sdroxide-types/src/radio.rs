//! Persisted radio-backend configuration (`radio.json`): choose between a
//! SoapySDR device and a CAT-controlled rig whose audio arrives over a USB
//! sound card. Serde-only — no I/O, safe in the wasm client (the settings UI
//! is shared, even though the CAT machinery is native-only).

use serde::{Deserialize, Serialize};

/// Which radio backend to drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Backend {
    /// Legacy "SoapySDR if present, else CAT" auto-detect. No longer offered in
    /// the UI, but kept so older `radio.json` files still deserialize.
    Auto,
    #[default]
    Soapy,
    Cat,
    /// OpenHPSDR ethernet SDR (Protocol 2), discovered/reached over the LAN.
    Hpsdr,
    /// TCI (Transceiver Control Interface) over WebSocket — ExpertSDR3, Thetis, …
    Tci,
    /// RTL2832U dongle driven directly over USB by the native driver — no
    /// SoapySDR, no libusb, nothing to install.
    RtlSdr,
    /// FlexRadio SmartSDR (FLEX-6000 / FLEX-8000) over its TCP API + VITA-49.
    Flex,
    /// Icom over LAN/WLAN (IC-705, IC-7610, IC-9700) — CI-V and audio over UDP.
    Icom,
}

impl Backend {
    pub const ALL: [Backend; 8] = [
        Backend::Auto,
        Backend::Soapy,
        Backend::Cat,
        Backend::Hpsdr,
        Backend::Tci,
        Backend::RtlSdr,
        Backend::Flex,
        Backend::Icom,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Backend::Auto => "Auto-detect (SoapySDR / CAT)",
            Backend::Soapy => "SoapySDR",
            Backend::Cat => "CAT / Audio",
            Backend::Hpsdr => "HPSDR (network)",
            Backend::Tci => "TCI (network)",
            Backend::RtlSdr => "RTL-SDR (USB)",
            Backend::Flex => "FlexRadio (network)",
            Backend::Icom => "Icom (network)",
        }
    }
}

/// CAT protocol family. Only `Xiegu` is hardware-verified so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum CatFamily {
    #[default]
    Xiegu,
    Icom,
    Yaesu,
}

impl CatFamily {
    pub const ALL: [CatFamily; 3] = [CatFamily::Xiegu, CatFamily::Icom, CatFamily::Yaesu];
    pub fn label(self) -> &'static str {
        match self {
            CatFamily::Xiegu => "Xiegu",
            CatFamily::Icom => "Icom",
            CatFamily::Yaesu => "Yaesu",
        }
    }
}

/// How the radio's audio is carried over the sound card.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SoundFormat {
    /// Stereo L=I, R=Q complex baseband → normal wideband engine path.
    Iq,
    /// Mono already-demodulated audio → audio-band panadapter (engine bypass).
    #[default]
    DemodAudio,
}

impl SoundFormat {
    pub const ALL: [SoundFormat; 2] = [SoundFormat::DemodAudio, SoundFormat::Iq];
    pub fn label(self) -> &'static str {
        match self {
            SoundFormat::Iq => "IQ (stereo)",
            SoundFormat::DemodAudio => "Demod audio",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Parity {
    #[default]
    None,
    Even,
    Odd,
}

impl Parity {
    pub const ALL: [Parity; 3] = [Parity::None, Parity::Even, Parity::Odd];
    pub fn label(self) -> &'static str {
        match self {
            Parity::None => "None",
            Parity::Even => "Even",
            Parity::Odd => "Odd",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum StopBits {
    #[default]
    One,
    Two,
}

impl StopBits {
    pub const ALL: [StopBits; 2] = [StopBits::One, StopBits::Two];
    pub fn label(self) -> &'static str {
        match self {
            StopBits::One => "1",
            StopBits::Two => "2",
        }
    }
}

/// A serial control line forced to a fixed level while the port is open (some
/// rigs need DTR/RTS held high to enable CAT). `None` = leave as-is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum LineState {
    #[default]
    None,
    High,
    Low,
}

impl LineState {
    pub const ALL: [LineState; 3] = [LineState::None, LineState::High, LineState::Low];
    pub fn label(self) -> &'static str {
        match self {
            LineState::None => "None",
            LineState::High => "High",
            LineState::Low => "Low",
        }
    }
}

/// How to key the transmitter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PttMethod {
    /// Rig keys itself from TX audio; software just routes audio.
    Vox,
    Dtr,
    Rts,
    /// A CAT command keys the rig.
    #[default]
    Cat,
}

impl PttMethod {
    pub const ALL: [PttMethod; 4] =
        [PttMethod::Cat, PttMethod::Dtr, PttMethod::Rts, PttMethod::Vox];
    pub fn label(self) -> &'static str {
        match self {
            PttMethod::Vox => "VOX",
            PttMethod::Dtr => "DTR",
            PttMethod::Rts => "RTS",
            PttMethod::Cat => "CAT",
        }
    }
}

/// Who drives the rig's mode for ordinary modes (USB/LSB/CW/AM/FM/DIGU/DIGL).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ModeControl {
    /// The app commands the rig's mode over CAT to match the selected mode.
    #[default]
    Cat,
    /// The operator sets the mode on the radio; the app just follows it.
    Radio,
}

impl ModeControl {
    pub const ALL: [ModeControl; 2] = [ModeControl::Cat, ModeControl::Radio];
    pub fn label(self) -> &'static str {
        match self {
            ModeControl::Cat => "CAT",
            ModeControl::Radio => "Radio controlled",
        }
    }
}

/// What mode the rig should be in for the FT8/FT4 digital engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum DigiMode {
    /// Force the rig to USB.
    #[default]
    Usb,
    /// Force the rig to its DATA/PKT (USB-D) mode.
    Data,
    /// Leave the rig's mode as the operator set it.
    Radio,
}

impl DigiMode {
    pub const ALL: [DigiMode; 3] = [DigiMode::Usb, DigiMode::Data, DigiMode::Radio];
    pub fn label(self) -> &'static str {
        match self {
            DigiMode::Usb => "USB",
            DigiMode::Data => "DIGI",
            DigiMode::Radio => "Radio controlled",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SerialConfig {
    /// Serial device path (Linux/mac `/dev/tty…`, Windows `COMx`).
    pub path: String,
    pub baud: u32,
    pub data_bits: u8,
    pub parity: Parity,
    pub stop_bits: StopBits,
    pub force_rts: LineState,
    pub force_dtr: LineState,
}

impl Default for SerialConfig {
    fn default() -> Self {
        SerialConfig {
            path: String::new(),
            baud: 19200,
            data_bits: 8,
            parity: Parity::None,
            stop_bits: StopBits::One,
            force_rts: LineState::None,
            force_dtr: LineState::None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CatConfig {
    pub family: CatFamily,
    pub serial: SerialConfig,
    pub ptt: PttMethod,
    /// How often to poll the rig for its dial/mode (Hz).
    pub poll_hz: f32,
    /// Who controls the rig's mode for ordinary modes.
    pub mode_control: ModeControl,
    /// What mode the rig uses for the FT8/FT4 engine.
    pub digi_mode: DigiMode,
    /// Icom CI-V transceiver address (hex byte), e.g. 0x70 for many rigs.
    pub icom_radio_id: u8,
    pub format: SoundFormat,
    /// Displayed panadapter bandwidth for demod-audio mode (Hz).
    pub audio_bw_hz: f64,
}

impl Default for CatConfig {
    fn default() -> Self {
        CatConfig {
            family: CatFamily::default(),
            serial: SerialConfig::default(),
            ptt: PttMethod::default(),
            poll_hz: 5.0,
            mode_control: ModeControl::default(),
            digi_mode: DigiMode::default(),
            icom_radio_id: 0x70,
            format: SoundFormat::default(),
            audio_bw_hz: 4000.0,
        }
    }
}

/// Which accessory filter board is wired to a Hermes-Lite 2's J16 header, and
/// therefore how its seven open-collector outputs should be driven.
///
/// Those pins are general-purpose openHPSDR outputs, not filter-only: operators
/// also wire them to amplifier PTT, antenna relays and transverter switching.
/// Driving them from band data would start operating that hardware, so the
/// default leaves every one of them off and the operator says what is attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum HpsdrFilterBoard {
    /// Leave all seven outputs off — the safe default, and correct for a bare
    /// board with nothing on J16.
    #[default]
    None,
    /// N2ADR filter board: one-hot relay select, forwarded by the gateware over
    /// I2C to the board's MCP23008.
    N2adr,
}

impl HpsdrFilterBoard {
    pub const ALL: [HpsdrFilterBoard; 2] = [HpsdrFilterBoard::None, HpsdrFilterBoard::N2adr];

    pub fn label(self) -> &'static str {
        match self {
            HpsdrFilterBoard::None => "None — outputs stay off",
            HpsdrFilterBoard::N2adr => "N2ADR filter board",
        }
    }
}

/// OpenHPSDR (ethernet SDR) backend configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HpsdrConfig {
    /// Explicit target IP (e.g. "192.168.1.50"). When set, connect directly and
    /// skip discovery/selection.
    pub manual_ip: Option<String>,
    /// IP of the device picked from a discovery scan (persisted selection).
    pub selected_ip: Option<String>,
    /// DDC sample rate in Hz (48k, 96k, 192k, 384k, 768k, 1536k).
    pub sample_rate_hz: f64,
    /// Front-end LNA gain in dB applied when the radio is opened, on boards
    /// that have one (Hermes-Lite 2: −12…+48 dB). Adjust it live in
    /// Settings → Device; this is the value the rig starts at.
    #[serde(default = "HpsdrConfig::default_lna_gain_db")]
    pub lna_gain_db: f64,
    /// Accessory board on the Hermes-Lite 2's J16 header. Defaults to `None`,
    /// which leaves the open-collector outputs untouched.
    #[serde(default)]
    pub filter_board: HpsdrFilterBoard,
    /// Conjugate the board's I/Q, mirroring the spectrum about the tuned
    /// frequency, on transmit as well as receive so the two directions cannot
    /// disagree about which sideband they are on.
    ///
    /// **On by default**: a Hermes-Lite 2 needs it — verified on air, where
    /// without it FT8 produces no decodes at all and SSB comes out on the wrong
    /// sideband. A board that turns out not to need it can turn it off.
    ///
    /// Deliberately *not* named `swap_iq`, which is what the one release that
    /// defaulted it to off called it. Ignoring that older key is the migration:
    /// whether an operator had found the setting and switched it on, or had it
    /// saved as off without ever knowing it existed, they all land on the value
    /// that works.
    #[serde(default = "HpsdrConfig::default_invert_spectrum")]
    pub invert_spectrum: bool,
}

impl Default for HpsdrConfig {
    fn default() -> Self {
        HpsdrConfig {
            manual_ip: None,
            selected_ip: None,
            sample_rate_hz: 1_536_000.0,
            lna_gain_db: Self::default_lna_gain_db(),
            filter_board: HpsdrFilterBoard::None,
            invert_spectrum: Self::default_invert_spectrum(),
        }
    }
}

impl HpsdrConfig {
    /// Range of the Hermes-Lite 2 front-end gain, in dB.
    pub const LNA_GAIN_MIN_DB: f64 = -12.0;
    pub const LNA_GAIN_MAX_DB: f64 = 48.0;
    /// Name of the RX gain element the backend exposes for that gain. Lives here
    /// rather than in `sdroxide-hpsdr` so the (wasm-safe) settings UI can address
    /// the same element without depending on the native backend crate.
    pub const LNA_GAIN_ELEMENT: &'static str = "LNA";

    /// Mid-scale default: sensitive enough on a quiet band without clipping the
    /// ADC on a real antenna.
    pub fn default_lna_gain_db() -> f64 {
        20.0
    }

    /// Hermes-Lite 2 boards deliver a conjugated stream, so inversion is the
    /// working default. See [`HpsdrConfig::invert_spectrum`].
    pub fn default_invert_spectrum() -> bool {
        true
    }

    /// Supported DDC sample rates (Hz) for Protocol 2 boards.
    pub const SAMPLE_RATES: [f64; 6] =
        [48_000.0, 96_000.0, 192_000.0, 384_000.0, 768_000.0, 1_536_000.0];

    /// Protocol 1 (Metis) boards top out at 384 kHz.
    pub const P1_SAMPLE_RATES: [f64; 4] = [48_000.0, 96_000.0, 192_000.0, 384_000.0];

    /// The sample rates valid for a given protocol (1 or 2).
    pub fn rates_for(protocol: u8) -> &'static [f64] {
        if protocol == 1 { &Self::P1_SAMPLE_RATES } else { &Self::SAMPLE_RATES }
    }

    /// Resolve the IP to connect to: manual override, else the persisted pick.
    /// `None` means "discover and use the first responder".
    pub fn target_ip(&self) -> Option<&str> {
        self.manual_ip.as_deref().filter(|s| !s.trim().is_empty()).or(self.selected_ip.as_deref())
    }
}

/// One HPSDR device found by a discovery scan. Wasm-safe so it can cross the
/// `RadioController` trait to the settings UI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HpsdrDevice {
    pub ip: String,
    pub mac: String,
    /// Board name, e.g. "Hermes", "Saturn", "Hermes-Lite 2".
    pub board: String,
    /// OpenHPSDR protocol the board speaks (1 or 2).
    pub protocol: u8,
    /// Whether the board reports it is already in use by another host.
    pub in_use: bool,
}

impl HpsdrDevice {
    /// One-line label for the selection UI.
    pub fn label(&self) -> String {
        let mut s = format!("{}  {}  (P{})", self.board, self.ip, self.protocol);
        if self.in_use {
            s.push_str("  [in use]");
        }
        if !self.supported() {
            s.push_str("  [unsupported protocol]");
        }
        s
    }

    /// Whether this device can be driven by the current implementation
    /// (Protocol 1 and Protocol 2 are both supported).
    pub fn supported(&self) -> bool {
        matches!(self.protocol, 1 | 2)
    }
}

/// TCI (Transceiver Control Interface, WebSocket) backend configuration.
/// Receive is wideband IQ (sdroxide demodulates); transmit is audio.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TciConfig {
    /// TCI server `host:port` (default `127.0.0.1:50001`, the ExpertSDR3 port).
    pub address: String,
    /// IQ stream sample rate in Hz (48k / 96k / 192k).
    pub iq_sample_rate_hz: f64,
}

impl Default for TciConfig {
    fn default() -> Self {
        TciConfig { address: "127.0.0.1:50001".into(), iq_sample_rate_hz: 192_000.0 }
    }
}

impl TciConfig {
    /// IQ sample rates offered in the UI.
    pub const IQ_RATES: [f64; 3] = [48_000.0, 96_000.0, 192_000.0];
}

/// How an RTL-SDR reaches HF. The R82xx tuner itself starts at 24 MHz, so
/// anything below that needs help from the dongle's hardware.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum RtlSdrHfMode {
    /// Tuner only — nothing below 24 MHz.
    Off,
    /// Use whatever this dongle has: the V4's built-in upconverter, or
    /// direct sampling on a V3. Switched automatically at the crossover.
    #[default]
    Auto,
    /// Force direct sampling on the ADC's Q branch (the V3's HF port). Has no
    /// meaning on a Blog V4, which upconverts instead.
    DirectQ,
}

impl RtlSdrHfMode {
    pub const ALL: [RtlSdrHfMode; 3] =
        [RtlSdrHfMode::Auto, RtlSdrHfMode::Off, RtlSdrHfMode::DirectQ];

    /// Paired with [`RtlSdrHfMode::from_code`] so the mode can ride the
    /// `HFMODE` pseudo-element; keep the two in step.
    pub fn code(self) -> u8 {
        match self {
            RtlSdrHfMode::Off => 0,
            RtlSdrHfMode::Auto => 1,
            RtlSdrHfMode::DirectQ => 2,
        }
    }

    pub fn from_code(code: u8) -> RtlSdrHfMode {
        match code {
            0 => RtlSdrHfMode::Off,
            2 => RtlSdrHfMode::DirectQ,
            _ => RtlSdrHfMode::Auto,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            RtlSdrHfMode::Off => "Off (tuner only, 24 MHz up)",
            RtlSdrHfMode::Auto => "Automatic",
            RtlSdrHfMode::DirectQ => "Direct sampling (Q branch)",
        }
    }
}

/// Which automatic gain loops to enable. The tuner AGC lives in the R82xx; the
/// RTL AGC is the demod's digital one. They are independent and can both run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum RtlSdrAgc {
    /// Manual tuner gain, no automatic loops — the setting for measurement and
    /// for weak-signal digital modes.
    #[default]
    Manual,
    Tuner,
    Rtl,
    Both,
}

impl RtlSdrAgc {
    pub const ALL: [RtlSdrAgc; 4] =
        [RtlSdrAgc::Manual, RtlSdrAgc::Tuner, RtlSdrAgc::Rtl, RtlSdrAgc::Both];
    pub fn label(self) -> &'static str {
        match self {
            RtlSdrAgc::Manual => "Manual (no AGC)",
            RtlSdrAgc::Tuner => "Tuner AGC",
            RtlSdrAgc::Rtl => "RTL digital AGC",
            RtlSdrAgc::Both => "Tuner + RTL AGC",
        }
    }

    /// Whether the R82xx runs its own LNA/mixer gain loop.
    pub fn tuner_auto(self) -> bool {
        matches!(self, RtlSdrAgc::Tuner | RtlSdrAgc::Both)
    }

    /// Whether the demod's digital AGC runs.
    pub fn rtl_auto(self) -> bool {
        matches!(self, RtlSdrAgc::Rtl | RtlSdrAgc::Both)
    }

    /// AGC mode as a number, so it can ride the existing `SetGain` command on
    /// the `AGC` pseudo-element instead of needing a new `Command` variant.
    /// Paired with [`RtlSdrAgc::from_code`]; keep the two in step.
    pub fn code(self) -> u8 {
        match self {
            RtlSdrAgc::Manual => 0,
            RtlSdrAgc::Tuner => 1,
            RtlSdrAgc::Rtl => 2,
            RtlSdrAgc::Both => 3,
        }
    }

    pub fn from_code(code: u8) -> RtlSdrAgc {
        match code {
            1 => RtlSdrAgc::Tuner,
            2 => RtlSdrAgc::Rtl,
            3 => RtlSdrAgc::Both,
            _ => RtlSdrAgc::Manual,
        }
    }
}

/// RTL-SDR (RTL2832U over USB) backend configuration. Receive only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RtlSdrConfig {
    /// USB serial of the dongle to open. `None` = the first one found. Serial
    /// rather than an index because bus position changes on every replug, and
    /// a persisted index would attach to the wrong dongle.
    pub serial: Option<String>,
    /// Sample rate in Hz. The resampler only reaches 225–300 kHz and
    /// 900 kHz–3.2 MHz; everything between is rejected by the hardware.
    pub sample_rate_hz: f64,
    /// Crystal error in parts per million. Read it off the `clock error`
    /// line that `RUST_LOG=sdroxide_rtlsdr=debug` prints once the stream runs.
    pub ppm: i32,
    /// Tuner gain in dB when AGC is off. Snapped to the nearest step the
    /// hardware can actually produce.
    pub tuner_gain_db: f64,
    pub agc: RtlSdrAgc,
    pub hf_mode: RtlSdrHfMode,
    /// Bias tee: ~4.5 V DC on the antenna coax for a remote LNA. Off by
    /// default, and turned off again on a clean shutdown — it will damage a
    /// transceiver or anything DC-shorted on the other end of the cable.
    pub bias_tee: bool,
    /// Bulk transfers kept in flight (advanced). The default gives ~53 ms of
    /// hardware-side buffering at 2.4 Msps, twice the worst-case retune stall.
    pub transfers: u8,
    /// Size of each bulk transfer in KiB (advanced). Must stay a multiple of
    /// the endpoint's 512-byte packet.
    pub transfer_kib: u16,
}

impl Default for RtlSdrConfig {
    fn default() -> Self {
        RtlSdrConfig {
            serial: None,
            sample_rate_hz: 2_400_000.0,
            ppm: 0,
            tuner_gain_db: 30.0,
            agc: RtlSdrAgc::Manual,
            hf_mode: RtlSdrHfMode::Auto,
            bias_tee: false,
            transfers: 16,
            transfer_kib: 16,
        }
    }
}

impl RtlSdrConfig {
    /// Gain element names the backend exposes. They live here rather than in
    /// `sdroxide-rtlsdr` so the (wasm-safe) settings UI can address them
    /// without depending on the native backend crate — same reason as
    /// [`HpsdrConfig::LNA_GAIN_ELEMENT`].
    pub const TUNER_GAIN_ELEMENT: &'static str = "TUNER";
    pub const IF_GAIN_ELEMENT: &'static str = "IF";
    /// Pseudo-elements carrying settings that are not gains at all.
    ///
    /// These ride the existing `SetGain` command so that adding this backend
    /// needs no new `Command` variant, no `DeviceCaps` field and no engine
    /// change for four settings only one backend has. They are deliberately
    /// absent from `DeviceCaps::gains`, so nothing renders them as sliders —
    /// the RTL-SDR settings panel drives them directly. The encodings live
    /// beside the enums they carry ([`RtlSdrAgc::code`], `HfMode as u8`) so
    /// the two ends cannot drift.
    pub const AGC_ELEMENT: &'static str = "AGC";
    pub const PPM_ELEMENT: &'static str = "PPM";
    pub const HF_MODE_ELEMENT: &'static str = "HFMODE";
    pub const BIAS_TEE_ELEMENT: &'static str = "BIASTEE";

    /// Sample rates offered in the UI. All lie inside the resampler's upper
    /// window except 250 kHz, which is in the lower one. 3.2 Msps is offered
    /// but drops samples on most hosts.
    pub const SAMPLE_RATES: [f64; 9] = [
        250_000.0,
        960_000.0,
        1_024_000.0,
        1_200_000.0,
        1_536_000.0,
        1_800_000.0,
        2_048_000.0,
        2_400_000.0,
        3_200_000.0,
    ];

    /// Maximum R82xx tuner gain, in dB (the last entry of the gain table).
    pub const GAIN_MAX_DB: f64 = 49.6;

    /// Below this, HF handling kicks in: the Blog V4's upconverter reference
    /// frequency, and equally the bottom of the R82xx's own range.
    pub const HF_CROSSOVER_HZ: f64 = 28_800_000.0;
}

/// One RTL-SDR dongle found on the USB bus. Wasm-safe so it can cross the
/// `RadioController` trait to the settings UI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RtlSdrDevice {
    /// USB serial string, when the dongle has one programmed.
    pub serial: Option<String>,
    /// Best available name: the USB product string, else the VID/PID table.
    pub name: String,
    pub vid: u16,
    pub pid: u16,
}

impl RtlSdrDevice {
    /// One-line label for the selection UI.
    pub fn label(&self) -> String {
        match &self.serial {
            Some(s) => format!("{}  (serial {s})", self.name),
            // Without a serial we can only ever open "the first one", so say so
            // rather than implying this entry can be pinned.
            None => format!("{}  [no serial — first match only]", self.name),
        }
    }
}

/// FlexRadio (SmartSDR) backend configuration. Receive is a wideband DAX IQ
/// stream (sdroxide demodulates); transmit is DAX audio the radio modulates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FlexConfig {
    /// Radio IP typed by the operator; empty means "use the discovered pick".
    pub manual_ip: Option<String>,
    /// IP chosen from the discovery list, remembered between sessions.
    pub selected_ip: Option<String>,
    /// DAX IQ stream sample rate in Hz (24k / 48k / 96k / 192k).
    pub iq_sample_rate_hz: f64,
    /// DAX IQ channel to claim (1..4). Pick a different one if another program
    /// already uses this channel on the same radio.
    pub daxiq_channel: u32,
    /// Antenna port for the slice we create (`ANT1`, `ANT2`, `RX_A`, …). Empty
    /// leaves the radio's own default.
    pub antenna: String,
    /// Station name shown in the radio's client list.
    pub station: String,
    /// Client id the radio assigned on the first connection, sent back on every
    /// later one. A GUI client's slices and panadapters are kept per client id,
    /// so reusing it means the radio hands our objects back instead of stranding
    /// them — otherwise every restart consumes another slice. Not shown in the
    /// UI; the radio owns the value, we only remember it.
    pub client_id: Option<String>,
}

impl Default for FlexConfig {
    fn default() -> Self {
        FlexConfig {
            manual_ip: None,
            selected_ip: None,
            iq_sample_rate_hz: 192_000.0,
            daxiq_channel: 1,
            antenna: String::new(),
            station: "sdroxide".into(),
            client_id: None,
        }
    }
}

impl FlexConfig {
    /// DAX IQ sample rates offered in the UI.
    pub const IQ_RATES: [f64; 4] = [24_000.0, 48_000.0, 96_000.0, 192_000.0];
    /// DAX IQ channels a radio provides.
    pub const CHANNELS: [u32; 4] = [1, 2, 3, 4];

    /// Resolve the IP to connect to: manual override, else the persisted pick.
    /// `None` means "discover and use the first radio found".
    pub fn target_ip(&self) -> Option<&str> {
        self.manual_ip.as_deref().filter(|s| !s.trim().is_empty()).or(self.selected_ip.as_deref())
    }
}

/// One FlexRadio found by a discovery listen. Wasm-safe so it can cross the
/// `RadioController` trait to the settings UI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlexDevice {
    pub ip: String,
    /// Model as the radio reports it, e.g. "FLEX-8600", "FLEX-6400M".
    pub model: String,
    pub serial: String,
    /// SmartSDR version running on the radio.
    pub version: String,
    /// Operator-assigned nickname.
    pub name: String,
    pub callsign: String,
    /// Whether a GUI client (SmartSDR, another sdroxide) already has the radio.
    pub in_use: bool,
}

impl FlexDevice {
    /// One-line label for the selection UI.
    pub fn label(&self) -> String {
        let mut s = format!("{}  {}", self.model, self.ip);
        if !self.name.is_empty() {
            s = format!("{}  \"{}\"", s, self.name);
        }
        if self.in_use {
            s.push_str("  [in use]");
        }
        s
    }
}

/// Icom network backend (IC-705 / IC-7610 / IC-9700 over LAN or WLAN). Control
/// is CI-V over UDP; audio is the radio's own stream — no cable, no sound card.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct IcomConfig {
    /// Radio address, e.g. `192.168.1.40`.
    pub ip: String,
    /// Network-control user configured on the radio.
    pub username: String,
    /// Its password. Icom's protocol only obfuscates this on the wire, so it is
    /// no more secret in transit than it is here.
    pub password: String,
    /// The radio's own model name; it checks the name against itself.
    pub model: String,
    /// CI-V transceiver address (0xA4 on an IC-705, 0x98 on an IC-7610).
    pub civ_address: u8,
    /// Displayed panadapter bandwidth for the audio-band view (Hz), used only
    /// while the radio's own scope is not running.
    pub audio_bw_hz: f64,
    /// Scope span to ask the radio for, as the ± value in Hz. The radio's own
    /// SPAN button overrides it; every sweep says what it covers.
    pub scope_span_hz: f64,
}

impl Default for IcomConfig {
    fn default() -> Self {
        IcomConfig {
            ip: String::new(),
            username: String::new(),
            password: String::new(),
            model: "IC-705".into(),
            civ_address: 0xA4,
            audio_bw_hz: 6000.0,
            scope_span_hz: 25_000.0,
        }
    }
}

impl IcomConfig {
    /// Models offered in the UI, with their usual CI-V address.
    pub const MODELS: [(&'static str, u8); 4] =
        [("IC-705", 0xA4), ("IC-7610", 0x98), ("IC-9700", 0xA2), ("IC-7850", 0x8E)];
}

/// Persisted backend configuration (`radio.json`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RadioConfig {
    pub backend: Backend,
    /// Sound-card device (cpal name) carrying the radio's RX audio → PC.
    pub radio_audio_in: Option<String>,
    /// Sound-card device (cpal name) carrying the TX audio PC → radio.
    pub radio_audio_out: Option<String>,
    pub cat: CatConfig,
    pub hpsdr: HpsdrConfig,
    pub tci: TciConfig,
    pub flex: FlexConfig,
    pub icom: IcomConfig,
    pub rtlsdr: RtlSdrConfig,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every way an existing `radio.json` can arrive has to land on the working
    /// sideband. The one release that shipped this setting called it `swap_iq`
    /// and defaulted it to off, which is the broken value — so that key is
    /// deliberately not read any more, and neither an operator who found the
    /// checkbox nor one who never knew it existed ends up inverted the wrong way.
    #[test]
    fn spectrum_inversion_survives_every_old_config_shape() {
        let cases = [
            // Written before the setting existed at all.
            r#"{"sample_rate_hz": 384000.0}"#,
            // The old key, left at its (broken) default by someone who never
            // opened the HPSDR settings.
            r#"{"sample_rate_hz": 384000.0, "swap_iq": false}"#,
            // The old key, switched on by an operator who diagnosed it.
            r#"{"sample_rate_hz": 384000.0, "swap_iq": true}"#,
            // A completely empty object.
            r#"{}"#,
        ];
        for json in cases {
            let cfg: HpsdrConfig = serde_json::from_str(json).expect("parses");
            assert!(cfg.invert_spectrum, "inverted after loading {json}");
        }
        // A fresh install gets it too.
        assert!(HpsdrConfig::default().invert_spectrum);
        // And an operator who turns it off is still obeyed on the next load.
        let off: HpsdrConfig =
            serde_json::from_str(r#"{"invert_spectrum": false}"#).expect("parses");
        assert!(!off.invert_spectrum);
    }

    #[test]
    fn hpsdr_defaults_round_trip() {
        let cfg = HpsdrConfig::default();
        let back: HpsdrConfig =
            serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(cfg, back);
    }
}
