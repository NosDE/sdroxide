//! FT8/FT4 digital-mode domain types, shared by the native engine, the
//! wire protocol, and the UI (native + WASM). Pure data + serde + pure
//! formatters — no mfsk-core here (that GPL dependency lives only in the
//! native `sdroxide-digi` crate).

use serde::{Deserialize, Serialize};

use crate::Mode;
use crate::entity::resolve_callsign;
use crate::geo::grid_distance_km;

/// One decoded FT8/FT4 message from a receive slot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decode {
    /// Unix seconds at the start of the slot this was decoded from.
    pub slot_utc: i64,
    /// WSJT-X-compatible SNR estimate (dB).
    pub snr_db: i16,
    /// Time offset from the nominal slot start (seconds).
    pub dt: f32,
    /// Audio tone offset within the passband (Hz, ~200..3000).
    pub audio_hz: f32,
    /// Full decoded message text, e.g. "CQ AB1CD FN42".
    pub message: String,
    /// Parsed recipient callsign ("CQ" appears as `is_cq`, not here).
    pub to: Option<String>,
    /// Parsed sender callsign.
    pub from: Option<String>,
    /// Parsed 4-char grid, if the payload was a grid locator.
    pub grid: Option<String>,
    /// True when the message is a CQ call.
    pub is_cq: bool,
    /// True when the CQ carried the `DX` modifier ("CQ DX AB1CD FN42") — the
    /// caller only wants stations outside their own DXCC entity. See
    /// [`cq_is_for_us`].
    #[serde(default)]
    pub cq_dx: bool,
    /// True for a 13-character free-text message. It carries no addressing at
    /// all — `to` and `from` are always `None`, however much the text may look
    /// like an exchange.
    #[serde(default)]
    pub free_text: bool,
}

/// How far away a station has to be to count as DX when the DXCC entity can't
/// be resolved from either callsign — a rough stand-in for "another country".
const DX_FALLBACK_KM: f64 = 3000.0;

/// True when a decoded CQ is one *we* may answer.
///
/// A plain CQ is open to everyone. "CQ DX" asks for stations outside the
/// caller's own DXCC entity, so it only counts as a CQ for us when we really are
/// DX for them: the UI neither colours nor lists such a call under "CQ only"
/// when we'd just be a local answering a DX call.
///
/// When the entity can't be resolved on both sides we fall back to the
/// great-circle distance between the grids, and if that is unavailable too the
/// call is treated as open — showing a CQ we can't judge beats hiding one we
/// could have worked.
pub fn cq_is_for_us(d: &Decode, my_call: &str, my_grid: &str) -> bool {
    if !d.is_cq {
        return false;
    }
    if !d.cq_dx {
        return true;
    }
    let Some(from) = d.from.as_deref() else { return true };
    match (resolve_callsign(from), resolve_callsign(my_call)) {
        // The DXCC entity is what "DX" means on HF: same entity → we're local.
        (Some(theirs), Some(mine)) => theirs.name != mine.name,
        _ => match d.grid.as_deref().and_then(|g| grid_distance_km(my_grid, g)) {
            Some(km) => km >= DX_FALLBACK_KM,
            None => true,
        },
    }
}

/// Where a QSO is in the standard FT8/FT4 exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QsoStep {
    Idle,
    /// We picked a non-CQ station and are holding until they call CQ (or call
    /// us) before we start transmitting, so we don't jump into their exchange.
    WaitCq,
    /// We are calling CQ, waiting for an answer.
    CallingCq,
    /// (Answerer) we replied to a CQ with our grid, awaiting their report.
    TxGrid,
    /// We are sending them a signal report.
    TxReport,
    /// We are sending R + their report.
    TxRReport,
    /// We are sending RR73.
    TxRr73,
    /// We are sending 73.
    Tx73,
    /// The exchange is complete and logged, but we keep the contact live for a
    /// few minutes and re-send our final message if the DX repeats theirs (i.e.
    /// they didn't receive our 73 / RR73).
    Confirming,
    /// QSO finished (logged).
    Done,
}

impl QsoStep {
    pub fn label(self) -> &'static str {
        match self {
            QsoStep::Idle => "Idle",
            QsoStep::WaitCq => "Wait CQ",
            QsoStep::CallingCq => "Calling CQ",
            QsoStep::TxGrid => "Tx Grid",
            QsoStep::TxReport => "Tx Report",
            QsoStep::TxRReport => "Tx R+Report",
            QsoStep::TxRr73 => "Tx RR73",
            QsoStep::Tx73 => "Tx 73",
            QsoStep::Confirming => "Confirming",
            QsoStep::Done => "Done",
        }
    }
}

/// One line of the current QSO's message exchange.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptLine {
    /// True = we transmitted it, false = we received it.
    pub tx: bool,
    pub text: String,
    /// True for a note about the station we called working *someone else* —
    /// overheard traffic, not part of our own exchange. The UI colours these
    /// differently so it's obvious they aren't talking to us.
    #[serde(default)]
    pub overheard: bool,
}

impl TranscriptLine {
    /// A message we transmitted.
    pub fn sent(text: impl Into<String>) -> Self {
        TranscriptLine { tx: true, text: text.into(), overheard: false }
    }

    /// A message we received as part of our exchange.
    pub fn rcvd(text: impl Into<String>) -> Self {
        TranscriptLine { tx: false, text: text.into(), overheard: false }
    }

    /// A note about the DX working another station.
    pub fn note(text: impl Into<String>) -> Self {
        TranscriptLine { tx: false, text: text.into(), overheard: true }
    }
}

/// Live status of the digital-mode engine, broadcast to clients.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DigiStatus {
    pub mode: Mode,
    pub step: QsoStep,
    /// The station we're working, if any.
    pub dx_call: Option<String>,
    pub dx_grid: Option<String>,
    /// Whether we'll key the next eligible slot.
    pub tx_next: bool,
    /// The exact message text queued for the next transmission.
    pub tx_pending_msg: Option<String>,
    /// Our transmit tone offset (Hz).
    pub audio_hz: f32,
    /// Which slot period we transmit in (true = even minute-second).
    pub tx_even: bool,
    /// True while a burst is currently on the air.
    pub transmitting: bool,
    /// The transmit watchdog stopped the sequencer. Cleared by any operator
    /// action (calling CQ, replying, picking a message).
    #[serde(default)]
    pub tx_watchdog: bool,
    /// The current QSO's message exchange (empty when idle).
    pub transcript: Vec<TranscriptLine>,
    /// Current engine config (so a fresh client can populate its editor).
    pub config: DigiConfig,
    /// Continuous keyboard modes (PSK/RTTY): the rolling decoded RX text.
    #[serde(default)]
    pub text_rx: String,
    /// Continuous keyboard modes: how many characters of the operator's
    /// outgoing buffer have been transmitted (drives the green "sent" cursor).
    #[serde(default)]
    pub tx_sent: usize,
    /// FSQ directed layer: callsigns recently heard, most-recent first.
    #[serde(default)]
    pub fsq_heard: Vec<String>,
    /// FSQ directed layer: parsed directed/allcall messages (rolling, capped).
    #[serde(default)]
    pub fsq_messages: Vec<FsqMsg>,
    /// RADE digital voice: modem state, when that mode is active.
    #[serde(default)]
    pub rade: Option<RadeStatus>,
}

/// Live state of the RADE V1 modem.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct RadeStatus {
    /// The receiver is locked to a signal.
    pub sync: bool,
    /// SNR estimate in a 3 kHz noise bandwidth (meaningful while `sync`).
    pub snr_db: f32,
    /// Frequency offset of the received signal (meaningful while `sync`).
    pub freq_offset_hz: f32,
    /// Peak level of the decoded speech, 0..1 — drives the RX meter.
    pub rx_level: f32,
    /// End-of-over frames seen this session; a change means the far end
    /// finished an over.
    pub eoo_count: u64,
    /// Samples dropped between the engine and the decode thread. Should stay
    /// at zero; anything else means the machine can't keep up.
    pub dropped: u64,
}

/// One parsed FSQ directed (or ALLCALL) message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FsqMsg {
    /// Sender callsign (empty if it couldn't be parsed).
    pub from: String,
    /// Addressee: a callsign, `allcall`, or empty for undirected.
    pub to: String,
    /// The message body (after the `:` trigger).
    pub text: String,
    /// True when the message is addressed to this station (or ALLCALL).
    pub to_me: bool,
}

impl DigiStatus {
    /// An idle status carrying just the operator config — emitted at engine
    /// startup so a client can seed its config editor before any digital mode is
    /// entered.
    pub fn idle(config: DigiConfig) -> Self {
        DigiStatus {
            mode: Mode::Usb,
            step: QsoStep::Idle,
            dx_call: None,
            dx_grid: None,
            tx_next: false,
            tx_pending_msg: None,
            audio_hz: 1500.0,
            tx_even: config.tx_even,
            transmitting: false,
            tx_watchdog: false,
            transcript: Vec::new(),
            config,
            text_rx: String::new(),
            tx_sent: 0,
            fsq_heard: Vec::new(),
            fsq_messages: Vec::new(),
            rade: None,
        }
    }
}

/// A completed QSO, for the persistent logbook (digital or manual entry).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct QsoRecord {
    /// Stable logbook id (0 = unassigned; the UI assigns on first store).
    pub id: u64,
    pub call: String,
    pub grid: Option<String>,
    /// Signal report we sent them (FT8 dB, or RST like 59/599 for voice).
    pub rst_sent: Option<i16>,
    /// Signal report they sent us.
    pub rst_rcvd: Option<i16>,
    /// RF frequency (dial + audio) at log time.
    pub freq_hz: f64,
    pub mode: String,
    pub band: String,
    pub start_utc: i64,
    pub end_utc: i64,
    pub my_call: String,
    pub my_grid: String,
    /// Free-text note (manual entries, corrections).
    pub comment: String,

    // Extended fields: contesting, awards, QSL status. All
    // `#[serde(default)]` via the struct attribute, so older logs still load.
    
    /// Worked operator's name.
    pub name: String,
    /// Worked station's town / QTH.
    pub qth: String,
    /// Worked station's primary subdivision (US state, etc.).
    pub state: String,
    /// County (US), if known.
    pub county: String,
    /// DXCC entity / country name.
    pub country: String,
    /// DXCC entity number.
    pub dxcc: Option<u16>,
    /// CQ zone (1..40).
    pub cq_zone: Option<u8>,
    /// ITU zone (1..90).
    pub itu_zone: Option<u8>,
    /// Continent (NA/SA/EU/AF/AS/OC/AN).
    pub continent: String,
    /// IOTA reference (e.g. "EU-005").
    pub iota: String,
    /// Special activity group (POTA / SOTA / WWFF) and its reference — mapped to
    /// ADIF SIG / SIG_INFO.
    pub sig: String,
    pub sig_info: String,
    /// Transmit power in watts.
    pub tx_pwr: Option<f32>,
    /// Logging operator (when different from the station call).
    pub operator: String,
    /// Contest identifier (ADIF CONTEST_ID).
    pub contest_id: String,
    /// Received serial (contest).
    pub srx: Option<u32>,
    /// Sent serial (contest).
    pub stx: Option<u32>,
    /// Received exchange string (contest).
    pub srx_string: String,
    /// Sent exchange string (contest).
    pub stx_string: String,
    /// My station's subdivision / country / zones (contest + awards).
    pub my_state: String,
    pub my_country: String,
    pub my_dxcc: Option<u16>,
    pub my_cq_zone: Option<u8>,
    pub my_itu_zone: Option<u8>,

    // ── QSL / confirmation status ──
    pub lotw_sent: bool,
    pub lotw_rcvd: bool,
    pub eqsl_sent: bool,
    pub eqsl_rcvd: bool,
    /// Uploaded to the QRZ logbook.
    pub qrz_sent: bool,
    /// Uploaded to Club Log.
    pub clublog_sent: bool,
    /// Paper QSL card sent / received.
    pub qsl_sent: bool,
    pub qsl_rcvd: bool,
    /// QSL routing / manager.
    pub qsl_via: String,
}

impl QsoRecord {
    /// True when any confirmation (LoTW, eQSL, or paper card) has been received.
    pub fn is_confirmed(&self) -> bool {
        self.lotw_rcvd || self.eqsl_rcvd || self.qsl_rcvd
    }
}

/// True if `log` already contains a QSO with the same callsign on `band`
/// (optionally the same `mode`) — a "worked before" / contest-dupe check.
/// `mode` empty matches any mode. `exclude_id` skips the record being edited.
pub fn worked_before(
    log: &[QsoRecord],
    call: &str,
    band: &str,
    mode: &str,
    exclude_id: u64,
) -> bool {
    let call = call.trim();
    if call.is_empty() {
        return false;
    }
    log.iter().any(|q| {
        q.id != exclude_id
            && q.call.eq_ignore_ascii_case(call)
            && q.band.eq_ignore_ascii_case(band)
            && (mode.is_empty() || q.mode.eq_ignore_ascii_case(mode))
    })
}

/// Operator configuration for digital-mode operation. Persisted engine-side,
/// THOR (DominoEX-family) submode: sets the symbol rate. All use 18 tones with
/// incremental frequency keying (IFK+) and convolutional FEC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ThorMode {
    Thor4,
    Thor8,
    Thor11,
    #[default]
    Thor16,
    Thor22,
    Thor32,
}

impl ThorMode {
    pub const ALL: [ThorMode; 6] = [
        ThorMode::Thor4,
        ThorMode::Thor8,
        ThorMode::Thor11,
        ThorMode::Thor16,
        ThorMode::Thor22,
        ThorMode::Thor32,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ThorMode::Thor4 => "THOR4",
            ThorMode::Thor8 => "THOR8",
            ThorMode::Thor11 => "THOR11",
            ThorMode::Thor16 => "THOR16",
            ThorMode::Thor22 => "THOR22",
            ThorMode::Thor32 => "THOR32",
        }
    }

    /// Nominal symbol rate (baud). The modem derives the tone spacing from this.
    pub fn baud(self) -> f32 {
        match self {
            ThorMode::Thor4 => 3.90625,
            ThorMode::Thor8 => 7.8125,
            ThorMode::Thor11 => 10.766,
            ThorMode::Thor16 => 15.625,
            ThorMode::Thor22 => 21.53,
            ThorMode::Thor32 => 31.25,
        }
    }
}

/// echoed to clients in [`DigiStatus`]. `#[serde(default)]` so an older
/// `digi.json` without the newer fields still loads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DigiConfig {
    pub my_call: String,
    pub my_grid: String,
    /// Default transmit period for CQ (true = even).
    pub tx_even: bool,
    /// Auto-advance the QSO through its steps (vs. manual button presses).
    pub auto_seq: bool,
    // Message templates. Placeholders: {MYCALL} {MYGRID} {DX} {REPORT}.
    pub msg_cq: String,
    pub msg_grid: String,
    pub msg_report: String,
    pub msg_rreport: String,
    pub msg_rr73: String,
    pub msg_73: String,
    /// RTTY baud rate (45.45 / 50 / 75).
    pub rtty_baud: f32,
    /// RTTY frequency shift in Hz (170 / 425 / 850).
    pub rtty_shift_hz: f32,
    /// Olivia tone count (2 / 4 / 8 / 16 / 32 / 64).
    pub olivia_tones: u8,
    /// Olivia bandwidth in Hz (125 / 250 / 500 / 1000 / 2000).
    pub olivia_bw_hz: f32,
    /// THOR submode (symbol rate).
    pub thor_mode: ThorMode,
    /// FSQ speed / baud (2 / 3 / 4.5 / 6).
    pub fsq_baud: f32,
    /// FSQ station callsign for directed (FSQCALL) messaging. Falls back to
    /// `my_call` when empty.
    pub fsq_call: String,
    /// Keyboard-mode decode squelch (0 = open/decode everything, 1 = only strong
    /// signals). Suppresses decoding of pure noise when no signal is present.
    pub digi_squelch: f32,
    /// SSTV transmit clock trim in parts-per-million. Stretches (+) or compresses
    /// (−) the image time-scale to null out slant against a receiver whose sound-
    /// card clock differs from this station's. 0 = no correction.
    pub sstv_tx_ppm: f32,
    /// RF Paint scan speed as a fraction of the base rate (1.0 = base/fastest,
    /// 0.25 = default = quarter speed / 4× slower). Lower scans the text/image
    /// more slowly, giving the receiver's waterfall more lines to render it.
    pub rf_paint_speed: f32,
    /// FT8/FT4: stop transmitting after this many minutes with no progress —
    /// no reply, no operator action. The guard against a station left calling
    /// into an empty band for hours. 0 disables it.
    pub tx_watchdog_min: u32,
    /// FT8/FT4: give up on a station after this many unanswered transmissions.
    /// Calling CQ is exempt (repeating a CQ is the point); this counts calls to
    /// one station that never comes back. 0 disables it.
    pub max_tx_repeats: u32,
    /// RADE: silence the demodulated (analog) audio, so only decoded speech is
    /// audible. Off by default — hearing the raw signal is how the operator
    /// tunes onto an over before the modem syncs.
    pub rade_mute_analog: bool,
}

impl Default for DigiConfig {
    fn default() -> Self {
        DigiConfig {
            my_call: String::new(),
            my_grid: String::new(),
            tx_even: true,
            auto_seq: true,
            msg_cq: "CQ {MYCALL} {MYGRID}".into(),
            msg_grid: "{DX} {MYCALL} {MYGRID}".into(),
            msg_report: "{DX} {MYCALL} {REPORT}".into(),
            msg_rreport: "{DX} {MYCALL} R{REPORT}".into(),
            msg_rr73: "{DX} {MYCALL} RR73".into(),
            msg_73: "{DX} {MYCALL} 73".into(),
            rtty_baud: 45.45,
            rtty_shift_hz: 170.0,
            olivia_tones: 32,
            olivia_bw_hz: 1000.0,
            thor_mode: ThorMode::Thor16,
            fsq_baud: 4.5,
            fsq_call: String::new(),
            digi_squelch: 0.35,
            tx_watchdog_min: 6,
            max_tx_repeats: 10,
            sstv_tx_ppm: 0.0,
            rf_paint_speed: 0.25,
            rade_mute_analog: false,
        }
    }
}

impl DigiConfig {
    /// Fill a template's placeholders. `report` is a signed dB value.
    pub fn fill(template: &str, my_call: &str, my_grid: &str, dx: &str, report: Option<i16>) -> String {
        let rpt = report.map(fmt_report).unwrap_or_default();
        template
            .replace("{MYCALL}", my_call)
            .replace("{MYGRID}", my_grid)
            .replace("{DX}", dx)
            .replace("{REPORT}", &rpt)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Format an SNR as an FT8 report token: `-13`, `+02`, `+00`.
pub fn fmt_report(db: i16) -> String {
    if db < 0 {
        format!("-{:02}", -db)
    } else {
        format!("+{:02}", db)
    }
}

/// Which band a frequency falls in, as an ADIF band string (e.g. "20m").
pub fn adif_band(freq_hz: f64) -> &'static str {
    let mhz = freq_hz / 1e6;
    match mhz {
        m if m < 2.0 => "160m",
        m if m < 4.0 => "80m",
        m if m < 5.5 => "60m",
        m if m < 7.3 => "40m",
        m if m < 10.5 => "30m",
        m if m < 14.5 => "20m",
        m if m < 18.2 => "17m",
        m if m < 21.5 => "15m",
        m if m < 25.0 => "12m",
        m if m < 29.8 => "10m",
        m if m < 54.0 => "6m",
        m if m < 148.0 => "2m",
        m if m < 450.0 => "70cm",
        _ => "23cm",
    }
}

// ── Log formatters (pure, unit-tested; run on any client, native or wasm) ──

/// Split a Unix timestamp into UTC `(year, month, day, hour, min, sec)`.
pub fn utc_ymd_hms(unix: i64) -> (i64, u32, u32, u32, u32, u32) {
    // Civil-from-days (Howard Hinnant's algorithm), no chrono dependency.
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (h, mi, s) = ((secs / 3600) as u32, ((secs % 3600) / 60) as u32, (secs % 60) as u32);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d, h, mi, s)
}

/// Inverse of [`utc_ymd_hms`]: a UTC civil date/time to a Unix timestamp
/// (days-from-civil, Howard Hinnant's algorithm). Inputs are clamped-ish by
/// the caller; out-of-range months/days still produce a deterministic value.
pub fn ymd_hms_to_unix(y: i64, m: u32, d: u32, h: u32, mi: u32, s: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m as i64 - 3 } else { m as i64 + 9 };
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days * 86_400 + h as i64 * 3600 + mi as i64 * 60 + s as i64
}

fn adif_date_time(unix: i64) -> (String, String) {
    let (y, m, d, h, mi, s) = utc_ymd_hms(unix);
    (format!("{y:04}{m:02}{d:02}"), format!("{h:02}{mi:02}{s:02}"))
}

fn adif_field(name: &str, value: &str) -> String {
    format!("<{}:{}>{}", name, value.len(), value)
}

/// Render a session's QSOs as an ADIF (.adi) document importable into
/// standard logging software.
pub fn qso_log_to_adif(records: &[QsoRecord]) -> String {
    let mut out = String::from(
        "ADIF export from sdroxide\n<ADIF_VER:5>3.1.4\n<PROGRAMID:8>sdroxide\n<EOH>\n",
    );
    for r in records {
        let (date, time) = adif_date_time(r.start_utc);
        let (_, time_off) = adif_date_time(r.end_utc);
        out.push_str(&adif_field("CALL", &r.call));
        out.push_str(&adif_field("QSO_DATE", &date));
        out.push_str(&adif_field("TIME_ON", &time));
        out.push_str(&adif_field("TIME_OFF", &time_off));
        out.push_str(&adif_field("BAND", &r.band));
        out.push_str(&adif_field("MODE", &r.mode));
        out.push_str(&adif_field("FREQ", &format!("{:.6}", r.freq_hz / 1e6)));
        if let Some(g) = &r.grid {
            out.push_str(&adif_field("GRIDSQUARE", g));
        }
        if let Some(s) = r.rst_sent {
            out.push_str(&adif_field("RST_SENT", &s.to_string()));
        }
        if let Some(s) = r.rst_rcvd {
            out.push_str(&adif_field("RST_RCVD", &s.to_string()));
        }
        // Extended fields — written only when populated.
        let opt_str = |out: &mut String, name: &str, v: &str| {
            if !v.trim().is_empty() {
                out.push_str(&adif_field(name, v.trim()));
            }
        };
        opt_str(&mut out, "NAME", &r.name);
        opt_str(&mut out, "QTH", &r.qth);
        opt_str(&mut out, "STATE", &r.state);
        opt_str(&mut out, "CNTY", &r.county);
        opt_str(&mut out, "COUNTRY", &r.country);
        if let Some(v) = r.dxcc {
            out.push_str(&adif_field("DXCC", &v.to_string()));
        }
        if let Some(v) = r.cq_zone {
            out.push_str(&adif_field("CQZ", &v.to_string()));
        }
        if let Some(v) = r.itu_zone {
            out.push_str(&adif_field("ITUZ", &v.to_string()));
        }
        opt_str(&mut out, "CONT", &r.continent);
        opt_str(&mut out, "IOTA", &r.iota);
        opt_str(&mut out, "SIG", &r.sig);
        opt_str(&mut out, "SIG_INFO", &r.sig_info);
        if let Some(v) = r.tx_pwr {
            out.push_str(&adif_field("TX_PWR", &format!("{v}")));
        }
        opt_str(&mut out, "OPERATOR", &r.operator);
        opt_str(&mut out, "CONTEST_ID", &r.contest_id);
        if let Some(v) = r.srx {
            out.push_str(&adif_field("SRX", &v.to_string()));
        }
        if let Some(v) = r.stx {
            out.push_str(&adif_field("STX", &v.to_string()));
        }
        opt_str(&mut out, "SRX_STRING", &r.srx_string);
        opt_str(&mut out, "STX_STRING", &r.stx_string);
        opt_str(&mut out, "MY_STATE", &r.my_state);
        opt_str(&mut out, "MY_COUNTRY", &r.my_country);
        if let Some(v) = r.my_dxcc {
            out.push_str(&adif_field("MY_DXCC", &v.to_string()));
        }
        if let Some(v) = r.my_cq_zone {
            out.push_str(&adif_field("MY_CQ_ZONE", &v.to_string()));
        }
        if let Some(v) = r.my_itu_zone {
            out.push_str(&adif_field("MY_ITU_ZONE", &v.to_string()));
        }
        opt_str(&mut out, "QSL_VIA", &r.qsl_via);
        let yn = |out: &mut String, name: &str, v: bool| {
            if v {
                out.push_str(&adif_field(name, "Y"));
            }
        };
        yn(&mut out, "LOTW_QSL_SENT", r.lotw_sent);
        yn(&mut out, "LOTW_QSL_RCVD", r.lotw_rcvd);
        yn(&mut out, "EQSL_QSL_SENT", r.eqsl_sent);
        yn(&mut out, "EQSL_QSL_RCVD", r.eqsl_rcvd);
        yn(&mut out, "QSL_SENT", r.qsl_sent);
        yn(&mut out, "QSL_RCVD", r.qsl_rcvd);
        out.push_str(&adif_field("STATION_CALLSIGN", &r.my_call));
        out.push_str(&adif_field("MY_GRIDSQUARE", &r.my_grid));
        out.push_str("<EOR>\n");
    }
    out
}

/// Parse an ADIF (.adi) document into QSO records. Tolerant of unknown fields
/// (ignored) and of a missing/short header. Used both for importing external
/// logs and for ingesting downloaded QSL confirmations. The inverse of
/// [`qso_log_to_adif`] for the fields sdroxide round-trips.
pub fn adif_to_qso_log(adif: &str) -> Vec<QsoRecord> {
    let mut records = Vec::new();
    let mut fields: Vec<(String, String)> = Vec::new();
    let bytes = adif.as_bytes();
    let mut i = 0usize;
    let mut in_header = adif.to_ascii_uppercase().contains("<EOH>");
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        let Some(close) = adif[i..].find('>') else { break };
        let tag = &adif[i + 1..i + close];
        i += close + 1;
        let mut parts = tag.splitn(3, ':');
        let name = parts.next().unwrap_or("").trim().to_ascii_uppercase();
        if name == "EOH" {
            in_header = false;
            fields.clear();
            continue;
        }
        if name == "EOR" {
            if !fields.is_empty() {
                records.push(record_from_fields(&fields));
            }
            fields.clear();
            continue;
        }
        let len: usize = parts.next().and_then(|l| l.trim().parse().ok()).unwrap_or(0);
        // A field with no length (or a header tag) has no value payload.
        let value = if len > 0 && i + len <= bytes.len() {
            let v = adif[i..i + len].to_string();
            i += len;
            v
        } else {
            String::new()
        };
        if !in_header {
            fields.push((name, value));
        }
    }
    // A final record without a trailing <EOR> (some exporters omit it).
    if !fields.is_empty() {
        records.push(record_from_fields(&fields));
    }
    records
}

fn record_from_fields(fields: &[(String, String)]) -> QsoRecord {
    let get = |key: &str| fields.iter().find(|(k, _)| k == key).map(|(_, v)| v.trim().to_string());
    let mut r = QsoRecord::default();
    r.call = get("CALL").unwrap_or_default().to_ascii_uppercase();
    r.grid = get("GRIDSQUARE").filter(|s| !s.is_empty());
    r.rst_sent = get("RST_SENT").and_then(|s| s.parse().ok());
    r.rst_rcvd = get("RST_RCVD").and_then(|s| s.parse().ok());
    if let Some(f) = get("FREQ").and_then(|s| s.parse::<f64>().ok()) {
        r.freq_hz = f * 1e6;
    }
    r.mode = get("MODE").unwrap_or_default().to_ascii_uppercase();
    r.band = get("BAND").map(|b| b.to_ascii_lowercase()).unwrap_or_else(|| {
        if r.freq_hz > 0.0 { adif_band(r.freq_hz).to_string() } else { String::new() }
    });
    let date = get("QSO_DATE").unwrap_or_default();
    let time_on = get("TIME_ON").unwrap_or_default();
    r.start_utc = parse_adif_datetime(&date, &time_on);
    let time_off = get("TIME_OFF").unwrap_or_else(|| time_on.clone());
    r.end_utc = if time_off.is_empty() { r.start_utc } else { parse_adif_datetime(&date, &time_off) };
    r.my_call = get("STATION_CALLSIGN")
        .or_else(|| get("OPERATOR"))
        .unwrap_or_default()
        .to_ascii_uppercase();
    r.my_grid = get("MY_GRIDSQUARE").unwrap_or_default();
    // Extended fields.
    r.name = get("NAME").unwrap_or_default();
    r.qth = get("QTH").unwrap_or_default();
    r.state = get("STATE").unwrap_or_default();
    r.county = get("CNTY").unwrap_or_default();
    r.country = get("COUNTRY").unwrap_or_default();
    r.dxcc = get("DXCC").and_then(|s| s.parse().ok());
    r.cq_zone = get("CQZ").and_then(|s| s.parse().ok());
    r.itu_zone = get("ITUZ").and_then(|s| s.parse().ok());
    r.continent = get("CONT").unwrap_or_default();
    r.iota = get("IOTA").unwrap_or_default();
    r.sig = get("SIG").unwrap_or_default();
    r.sig_info = get("SIG_INFO").unwrap_or_default();
    r.tx_pwr = get("TX_PWR").and_then(|s| s.parse().ok());
    r.operator = get("OPERATOR").unwrap_or_default();
    r.contest_id = get("CONTEST_ID").unwrap_or_default();
    r.srx = get("SRX").and_then(|s| s.parse().ok());
    r.stx = get("STX").and_then(|s| s.parse().ok());
    r.srx_string = get("SRX_STRING").unwrap_or_default();
    r.stx_string = get("STX_STRING").unwrap_or_default();
    r.my_state = get("MY_STATE").unwrap_or_default();
    r.my_country = get("MY_COUNTRY").unwrap_or_default();
    r.my_dxcc = get("MY_DXCC").and_then(|s| s.parse().ok());
    r.my_cq_zone = get("MY_CQ_ZONE").and_then(|s| s.parse().ok());
    r.my_itu_zone = get("MY_ITU_ZONE").and_then(|s| s.parse().ok());
    r.qsl_via = get("QSL_VIA").unwrap_or_default();
    let yn = |key: &str| get(key).map(|v| v.eq_ignore_ascii_case("Y")).unwrap_or(false);
    r.lotw_sent = yn("LOTW_QSL_SENT");
    r.lotw_rcvd = yn("LOTW_QSL_RCVD");
    r.eqsl_sent = yn("EQSL_QSL_SENT");
    r.eqsl_rcvd = yn("EQSL_QSL_RCVD");
    r.qsl_sent = yn("QSL_SENT");
    r.qsl_rcvd = yn("QSL_RCVD");
    r
}

/// Parse an ADIF `YYYYMMDD` + `HHMM`/`HHMMSS` pair into a Unix timestamp.
fn parse_adif_datetime(date: &str, time: &str) -> i64 {
    if date.len() < 8 {
        return 0;
    }
    let y = date[0..4].parse().unwrap_or(1970);
    let mo = date[4..6].parse().unwrap_or(1);
    let d = date[6..8].parse().unwrap_or(1);
    let (h, mi, s) = if time.len() >= 6 {
        (time[0..2].parse().unwrap_or(0), time[2..4].parse().unwrap_or(0), time[4..6].parse().unwrap_or(0))
    } else if time.len() >= 4 {
        (time[0..2].parse().unwrap_or(0), time[2..4].parse().unwrap_or(0), 0)
    } else {
        (0, 0, 0)
    };
    ymd_hms_to_unix(y, mo, d, h, mi, s)
}

/// Render a session's QSOs as a human-readable text log.
pub fn qso_log_to_text(records: &[QsoRecord]) -> String {
    let mut out = String::from(
        "sdroxide QSO log\nUTC date/time        call       grid  freq(MHz)  mode  sent rcvd\n",
    );
    for r in records {
        let (date, time) = adif_date_time(r.start_utc);
        let d = format!("{}-{}-{} {}:{}:{}", &date[0..4], &date[4..6], &date[6..8],
            &time[0..2], &time[2..4], &time[4..6]);
        out.push_str(&format!(
            "{:19}  {:10} {:5} {:10.6}  {:4}  {:>4} {:>4}\n",
            d,
            r.call,
            r.grid.as_deref().unwrap_or("-"),
            r.freq_hz / 1e6,
            r.mode,
            r.rst_sent.map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
            r.rst_rcvd.map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_formatting() {
        assert_eq!(fmt_report(-13), "-13");
        assert_eq!(fmt_report(2), "+02");
        assert_eq!(fmt_report(0), "+00");
    }

    fn cq(msg: &str, from: &str, grid: Option<&str>) -> Decode {
        Decode {
            slot_utc: 0,
            snr_db: -10,
            dt: 0.1,
            audio_hz: 1500.0,
            message: msg.to_string(),
            to: None,
            from: Some(from.to_string()),
            grid: grid.map(|g| g.to_string()),
            is_cq: true,
            cq_dx: msg.starts_with("CQ DX"),
            free_text: false,
        }
    }

    #[test]
    fn cq_dx_is_only_for_stations_outside_the_callers_entity() {
        // A plain CQ is for everyone, near or far.
        let plain = cq("CQ W1AW FN31", "W1AW", Some("FN31"));
        assert!(cq_is_for_us(&plain, "K2XYZ", "FN30"));

        // "CQ DX" from a US station: another US station is not DX for them.
        let dx = cq("CQ DX W1AW FN31", "W1AW", Some("FN31"));
        assert!(!cq_is_for_us(&dx, "K2XYZ", "FN30"));
        // A German station is.
        assert!(cq_is_for_us(&dx, "DL1ABC", "JN48"));
        // So is Hawaii — same country, but a separate DXCC entity.
        assert!(cq_is_for_us(&dx, "KH6ABC", "BL11"));

        // Not a CQ at all → never counted as one.
        let mut qso = plain.clone();
        qso.is_cq = false;
        assert!(!cq_is_for_us(&qso, "DL1ABC", "JN48"));
    }

    #[test]
    fn unresolvable_cq_dx_falls_back_to_distance_then_to_showing_it() {
        // "QQ" is no country cty.dat knows; with grids on both sides the
        // great-circle distance decides instead.
        let mut dx = cq("CQ DX QQ1QQ JN48", "QQ1QQ", Some("JN48"));
        assert!(!cq_is_for_us(&dx, "QQ2QQ", "JN47"), "a neighbour is not DX");
        assert!(cq_is_for_us(&dx, "QQ2QQ", "FN31"), "an ocean away is DX");

        // No grid and no resolvable entity → we can't judge, so show it.
        dx.grid = None;
        assert!(cq_is_for_us(&dx, "QQ2QQ", "JN47"));
        // Nor can we judge with no station callsign or grid of our own.
        let dx = cq("CQ DX W1AW FN31", "W1AW", Some("FN31"));
        assert!(cq_is_for_us(&dx, "", ""));
    }

    #[test]
    fn template_fill_collapses_spaces() {
        let s = DigiConfig::fill("{DX} {MYCALL} {REPORT}", "AB1CD", "FN42", "W9XYZ", Some(-9));
        assert_eq!(s, "W9XYZ AB1CD -09");
        let cq = DigiConfig::fill("CQ {MYCALL} {MYGRID}", "AB1CD", "FN42", "", None);
        assert_eq!(cq, "CQ AB1CD FN42");
    }

    #[test]
    fn utc_conversion_matches_known_epoch() {
        // 2021-01-01 00:00:00 UTC = 1609459200
        assert_eq!(utc_ymd_hms(1_609_459_200), (2021, 1, 1, 0, 0, 0));
        // 2023-11-14 22:13:20 UTC = 1700000000
        assert_eq!(utc_ymd_hms(1_700_000_000), (2023, 11, 14, 22, 13, 20));
    }

    #[test]
    fn adif_has_required_fields() {
        let rec = QsoRecord {
            call: "W9XYZ".into(),
            grid: Some("EM48".into()),
            rst_sent: Some(-9),
            rst_rcvd: Some(-12),
            freq_hz: 14_074_000.0,
            mode: "FT8".into(),
            band: "20m".into(),
            start_utc: 1_609_459_200,
            end_utc: 1_609_459_260,
            my_call: "AB1CD".into(),
            my_grid: "FN42".into(),
            ..Default::default()
        };
        let adif = qso_log_to_adif(&[rec]);
        assert!(adif.contains("<CALL:5>W9XYZ"));
        assert!(adif.contains("<QSO_DATE:8>20210101"));
        assert!(adif.contains("<BAND:3>20m"));
        assert!(adif.contains("<MODE:3>FT8"));
        assert!(adif.contains("<GRIDSQUARE:4>EM48"));
        assert!(adif.contains("<EOR>"));
        assert_eq!(adif_band(14_074_000.0), "20m");
    }

    #[test]
    fn adif_import_round_trips() {
        let rec = QsoRecord {
            call: "W9XYZ".into(),
            grid: Some("EM48".into()),
            rst_sent: Some(-9),
            rst_rcvd: Some(-12),
            freq_hz: 14_074_000.0,
            mode: "FT8".into(),
            band: "20m".into(),
            start_utc: 1_609_459_200,
            end_utc: 1_609_459_260,
            my_call: "AB1CD".into(),
            my_grid: "FN42".into(),
            ..Default::default()
        };
        let adif = qso_log_to_adif(std::slice::from_ref(&rec));
        let back = adif_to_qso_log(&adif);
        assert_eq!(back.len(), 1);
        let b = &back[0];
        assert_eq!(b.call, "W9XYZ");
        assert_eq!(b.grid.as_deref(), Some("EM48"));
        assert_eq!(b.mode, "FT8");
        assert_eq!(b.band, "20m");
        assert_eq!(b.rst_sent, Some(-9));
        assert_eq!(b.rst_rcvd, Some(-12));
        assert_eq!(b.start_utc, 1_609_459_200);
        assert_eq!(b.my_call, "AB1CD");
        assert_eq!(b.my_grid, "FN42");
    }

    #[test]
    fn time_round_trips() {
        for &t in &[0i64, 1_609_459_260, 1_753_050_960, 2_000_000_000] {
            let (y, mo, d, h, mi, s) = utc_ymd_hms(t);
            assert_eq!(ymd_hms_to_unix(y, mo, d, h, mi, s), t, "round-trip {t}");
        }
        // A known civil date: 2021-01-01 00:01:00 UTC.
        assert_eq!(ymd_hms_to_unix(2021, 1, 1, 0, 1, 0), 1_609_459_260);
    }
}
