use serde::{Deserialize, Serialize};

use crate::{
    AgcMode, Band, DigiConfig, Direction, Mode, NetworkConfig, NrLevel, RxId, SkimmerSettings,
    QsoStep, RigctldConfig, SpectrumConfig, SstvMode, TciServerConfig, UploadTarget, Vfo,
    WsjtxConfig,
};

/// The single control vocabulary. The GUI, the WebSocket protocol, and the
/// future TCI server all speak `Command`; the DSP engine is its only consumer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Command {
    // VFO / tuning
    SetVfo { vfo: Vfo, hz: f64 },
    SelectVfo(Vfo),
    SwapVfos,
    CopyAtoB,
    SetSplit(bool),
    SetCenter(f64),
    SetSampleRate(f64),
    /// Engine applies band-stack recall (or the band default entry).
    SetBand(Band),

    // Receiver settings
    SetMode { rx: RxId, mode: Mode },
    SetFilter { rx: RxId, lo: f32, hi: f32 },
    SetAgc { rx: RxId, agc: AgcMode },
    SetAgcMaxGain { rx: RxId, db: f32 },
    SetVolume { rx: RxId, v: f32 },
    SetMute { rx: RxId, muted: bool },
    /// Squelch threshold in dBFS ([`crate::SQUELCH_OPEN_DB`] = open).
    SetSquelch { rx: RxId, db: f32 },
    SetNoiseBlanker(bool),
    /// Spectral audio noise-reduction intensity for a receiver.
    SetNoiseReduction { rx: RxId, level: NrLevel },
    /// Adaptive auto-notch (constant-tone canceller) for a receiver.
    SetAutoNotch { rx: RxId, on: bool },
    SetSubRx(bool),
    SetRit { enabled: bool, hz: i32 },
    SetXit { enabled: bool, hz: i32 },
    /// Start (`true`) or stop (`false`) recording the receiver audio to an MP3
    /// file. The engine names the file (date/time/frequency/mode) and stores it
    /// in the user's music directory (or the config dir as a fallback).
    SetRecording(bool),

    // Transmit
    SetPtt(bool),
    SetTune(bool),
    SetTxDrive(f32),
    SetTuneDrive(f32),
    SetMicGain(f32),

    // Voice keyer (10 recorded messages; see [`crate::VoiceStatus`])
    /// Start recording the microphone into a slot (`Some`), or stop and store
    /// what has been recorded (`None`). Refused while transmitting.
    VoiceRecord(Option<u8>),
    /// Transmit a recorded message (`Some`), or stop one in progress (`None`).
    /// The engine keys the transmitter itself and unkeys at the end of the
    /// message. In RADE the recording is fed to the codec in place of the
    /// microphone; in the other digital modes the keyer is refused.
    VoicePlay(Option<u8>),
    /// Listen to a recorded message through the speakers without transmitting
    /// (`Some`), or stop the monitor (`None`). Refused while transmitting.
    VoicePreview(Option<u8>),
    /// Erase a slot's recording.
    VoiceClear(u8),
    /// Rename a slot (the label the UI and the bindings editor show).
    VoiceRename { slot: u8, name: String },

    // Hardware
    SetGain { dir: Direction, element: String, db: f64 },
    SetAntenna { dir: Direction, name: String },
    /// Run a tune cycle on the radio's built-in antenna tuner. This transmits,
    /// so the engine applies the same rails as PTT. Progress and outcome arrive
    /// as [`crate::AtuState`] in the state snapshot.
    StartAtu,
    /// Take the built-in tuner out of circuit.
    BypassAtu,

    // Memories
    StoreMemory { name: String },
    RecallMemory(u32),
    DeleteMemory(u32),

    // Display
    SetSpectrumCfg(SpectrumConfig),

    // Digital modes (FT8/FT4)
    SetDigiConfig(DigiConfig),
    /// Set our transmit tone offset within the passband (Hz).
    SetDigiAudioFreq(f32),
    /// Start calling CQ.
    DigiCallCq,
    /// Begin a QSO with a decoded station. `wait_for_cq` holds transmission
    /// until the station calls CQ (or calls us) — set when replying to a decode
    /// that is neither a CQ nor addressed to us, so we don't jump into an
    /// exchange already in progress.
    DigiStartQso {
        from: String,
        grid: Option<String>,
        snr: i16,
        audio_hz: f32,
        #[serde(default)]
        wait_for_cq: bool,
    },
    /// FT8/FT4: jump the exchange to this step, choosing by hand which message
    /// goes out next (WSJT-X's Tx1–Tx6). Steps that address a station are
    /// ignored when none is being worked.
    DigiSetStep(QsoStep),
    /// FT8/FT4: send this message verbatim in the next transmit slot, then
    /// carry on with the exchange. Empty text cancels one queued but unsent.
    DigiSendText(String),
    /// Gracefully stop the QSO sequence (finish the current burst, then idle).
    DigiStopQso,
    /// Abort any in-progress transmission immediately.
    DigiAbortTx,
    /// Continuous keyboard modes (PSK/RTTY): set the full outgoing text buffer.
    /// The engine keeps already-sent characters and streams the rest.
    DigiTxText(String),
    /// Continuous keyboard modes: enter (true) or leave (false) transmit.
    DigiTxActive(bool),
    /// SSTV: select the mode (also sizes the TX image). `None` = Auto — the RX
    /// auto-detects the mode and TX defaults to Martin 1.
    SstvSetMode(Option<SstvMode>),
    /// SSTV: transmit a composed image (PNG bytes) in the given mode. Keying
    /// starts immediately; `DigiAbortTx` stops it.
    SstvTx { mode: SstvMode, png: Vec<u8> },
    /// FSQ image: transmit a picture (PNG bytes; the engine grayscales/scales it).
    DigiImageTx { png: Vec<u8> },

    // Skimmers
    /// Set which skimmers (CW / PSK / RTTY) run and how hard each squelches.
    SetSkimmerConfig(SkimmerSettings),

    // Network cockpit: spot feeds, lookups, uploads.
    /// Apply (and persist) the network-feature configuration: (re)connect the
    /// DX cluster, (dis)arm the POTA/SOTA/PSK feeds, and store credentials.
    SetNetworkConfig(NetworkConfig),
    /// The operator's current dial frequency, so band-scoped feeds (PSK
    /// Reporter) can query the right slice. Sent by the engine on VFO change.
    SpotDialHint(f64),
    /// Look up a callsign via the configured provider; the result comes back as
    /// [`crate::RadioEvent::CallsignResult`].
    LookupCallsign { call: String },
    /// Upload one QSO's ADIF to the given targets; each result comes back as
    /// [`crate::RadioEvent::Upload`].
    UploadQso { qso_id: u64, adif: String, targets: Vec<UploadTarget> },
    /// Download QSL confirmations from LoTW/eQSL and return the parsed
    /// confirmation records as [`crate::RadioEvent::Confirmations`].
    SyncConfirmations,

    /// Apply (and persist) the built-in TCI server configuration: bind, rebind
    /// or stop the listener that third-party TCI clients connect to. The result
    /// comes back as [`crate::RadioEvent::TciServerStatus`].
    SetTciServerConfig(TciServerConfig),

    /// Apply (and persist) the built-in Hamlib rigctld server configuration:
    /// bind, rebind or stop the listener that "NET rigctl" clients (WSJT-X,
    /// fldigi, N1MM, GPredict, …) connect to. The result comes back as
    /// [`crate::RadioEvent::RigctldStatus`].
    SetRigctldConfig(RigctldConfig),
    /// Apply (and persist) the WSJT-X UDP broadcast configuration: start, retarget
    /// or stop the datagram stream that GridTracker, JTAlert, N1MM+ and Log4OM
    /// listen to. Output only — nothing arrives on that socket.
    SetWsjtxConfig(WsjtxConfig),
}
