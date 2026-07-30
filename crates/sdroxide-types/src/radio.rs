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
    /// FlexRadio SmartSDR (FLEX-6000 / FLEX-8000) over its TCP API + VITA-49.
    Flex,
    /// Icom over LAN/WLAN (IC-705, IC-7610, IC-9700) — CI-V and audio over UDP.
    Icom,
}

impl Backend {
    pub const ALL: [Backend; 7] = [
        Backend::Auto,
        Backend::Soapy,
        Backend::Cat,
        Backend::Hpsdr,
        Backend::Tci,
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
    pub const ALL: [PttMethod; 4] = [PttMethod::Cat, PttMethod::Dtr, PttMethod::Rts, PttMethod::Vox];
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
}

impl Default for HpsdrConfig {
    fn default() -> Self {
        HpsdrConfig { manual_ip: None, selected_ip: None, sample_rate_hz: 1_536_000.0 }
    }
}

impl HpsdrConfig {
    /// Supported DDC sample rates (Hz) for Protocol 2 boards.
    pub const SAMPLE_RATES: [f64; 6] =
        [48_000.0, 96_000.0, 192_000.0, 384_000.0, 768_000.0, 1_536_000.0];

    /// Protocol 1 (Metis) boards top out at 384 kHz.
    pub const P1_SAMPLE_RATES: [f64; 4] = [48_000.0, 96_000.0, 192_000.0, 384_000.0];

    /// The sample rates valid for a given protocol (1 or 2).
    pub fn rates_for(protocol: u8) -> &'static [f64] {
        if protocol == 1 {
            &Self::P1_SAMPLE_RATES
        } else {
            &Self::SAMPLE_RATES
        }
    }

    /// Resolve the IP to connect to: manual override, else the persisted pick.
    /// `None` means "discover and use the first responder".
    pub fn target_ip(&self) -> Option<&str> {
        self.manual_ip
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .or(self.selected_ip.as_deref())
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
        self.manual_ip
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .or(self.selected_ip.as_deref())
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
}
