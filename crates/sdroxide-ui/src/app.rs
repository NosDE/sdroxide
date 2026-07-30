use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui::{self, Color32, ComboBox, DragValue, RichText, Slider};
use sdroxide_types::{
    AgcMode, AudioDevices, Band, CallsignInfo, Command, Decode, DeviceCaps, DigiStatus, Direction,
    LookupProvider, MemoryChannel, Meters, Mode, NetworkConfig, QsoRecord, RadioController,
    RadioEvent, RadioState, RxId, SkimmerKind, SkimmerSpot, SpectrumConfig, SpectrumFrame, Spot,
    SpotKind, SstvMode, SstvStatus, UploadResult, UploadTarget, Vfo,
};

use crate::theme::ThemedScroll;
use crate::view::ViewState;
use crate::widgets::{freq_display, smeter, spectrum_view};
use crate::{colormap, waterfall_gpu};

/// Viewport/FFT config updates are sent once the view has been stable this
/// long (seconds of egui time — `std::time::Instant` panics on wasm).
const CFG_DEBOUNCE_S: f64 = 0.25;

/// A skimmer box fades to nothing over this many seconds after its signal
/// stops keying, instead of vanishing.
const SKIMMER_FADE_SECS: f64 = 5.0;

/// FT8/FT4 callsign boxes stop being drawn once the newest decode is this old,
/// so a stalled decoder (dead band, band change) doesn't leave labels pinned to
/// the waterfall for good.
const FT8_LABEL_MAX_AGE_SECS: i64 = 45;

/// Settings dialog tabs: General (station identity + audio devices), the radio
/// interface and its settings, display/UI preferences, control inputs
/// (keyboard/mouse bindings), the network cockpit (spot feeds + uploads), and
/// the built-in TCI server.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsTab {
    General,
    Radio,
    Ui,
    Controls,
    Spots,
    FreeDv,
    Uploads,
    Servers,
}

/// Everything the settings dialog can change, collected in one place.
///
/// The window closure borrows `&self`, so `settings_body` can't reach
/// `&mut self.ctrl` — edits are written here and applied by `settings_window`
/// after the closure returns.
struct SettingsIo<'a> {
    iface_opts: &'a [sdroxide_types::Backend],
    radio_edit: &'a mut Option<sdroxide_types::RadioConfig>,
    audio_pick: &'a mut Option<(bool, Option<String>)>,
    hpsdr_discover: &'a mut bool,
    tci_test: &'a mut bool,
    flex_discover: &'a mut bool,
    flex_test: &'a mut bool,
    apply_iface: &'a mut bool,
    ui_edit: &'a mut sdroxide_types::UiSettings,
    digi_edit: &'a mut sdroxide_types::DigiConfig,
    digi_seeded: bool,
    net_edit: &'a mut NetworkConfig,
    net_cmds: &'a mut String,
    net_apply: &'a mut bool,
    net_sync: &'a mut bool,
    /// The built-in TCI *server* — this app acting as a rig for third-party
    /// clients, as opposed to the TCI client configured on the Radio tab.
    tci_srv_edit: &'a mut sdroxide_types::TciServerConfig,
    tci_srv_apply: &'a mut bool,
    /// The built-in Hamlib rigctld server — the control-only surface every
    /// "NET rigctl" client speaks.
    rigctld_edit: &'a mut sdroxide_types::RigctldConfig,
    rigctld_apply: &'a mut bool,
    /// The WSJT-X UDP broadcast — decodes, status and logged QSOs for
    /// GridTracker, JTAlert, N1MM+ and Log4OM.
    wsjtx_edit: &'a mut sdroxide_types::WsjtxConfig,
    wsjtx_apply: &'a mut bool,
    /// Control-input bindings, plus the row (if any) waiting to capture a
    /// keypress. Persisted on close, since a rebind has no APPLY step.
    input_edit: &'a mut sdroxide_types::InputSettings,
    key_capture: &'a mut Option<usize>,
    midi_learn: &'a mut Option<crate::input::MidiLearn>,
    midi_rescan: &'a mut bool,
    tab: &'a mut SettingsTab,
}

/// Repaint-poll cadence when no spectrum stream is flowing (startup, connection
/// lost, stalled stream) — the app truly idles between these wakes.
const IDLE_POLL_MS: u64 = 250;
/// The stream counts as stalled after this long without a new frame (seconds).
const STREAM_STALE_S: f64 = 1.0;

/// Stable per-callsign id for the FT8 overlay boxes (keeps a station's box in
/// place across slots).
fn hash_call(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Fill `dst` from `src` only when `dst` is blank and `src` is non-empty;
/// returns whether it changed anything.
fn fill_blank(dst: &mut String, src: Option<&str>) -> bool {
    if dst.trim().is_empty() {
        if let Some(v) = src.map(str::trim) {
            if !v.is_empty() {
                *dst = v.to_string();
                return true;
            }
        }
    }
    false
}

/// Which upload targets are enabled for auto-upload in `cfg`.
fn auto_upload_targets(cfg: &NetworkConfig) -> Vec<UploadTarget> {
    let mut t = Vec::new();
    if cfg.auto_upload_eqsl {
        t.push(UploadTarget::Eqsl);
    }
    if cfg.auto_upload_qrz {
        t.push(UploadTarget::QrzLogbook);
    }
    if cfg.auto_upload_clublog {
        t.push(UploadTarget::ClubLog);
    }
    t
}

/// Upload targets that have credentials configured (for the manual per-QSO
/// upload button).
fn configured_upload_targets(cfg: &NetworkConfig) -> Vec<UploadTarget> {
    let mut t = Vec::new();
    if !cfg.eqsl.user.trim().is_empty() {
        t.push(UploadTarget::Eqsl);
    }
    if !cfg.qrz_logbook_key.trim().is_empty() {
        t.push(UploadTarget::QrzLogbook);
    }
    if !cfg.clublog.user.trim().is_empty() && !cfg.clublog_api_key.trim().is_empty() {
        t.push(UploadTarget::ClubLog);
    }
    t
}

/// Single-record ADIF + targets for auto-upload of a freshly logged QSO, or
/// `None` when auto-upload is off or no target is enabled.
fn auto_upload_adif(cfg: &NetworkConfig, rec: &QsoRecord) -> Option<(u64, String, Vec<UploadTarget>)> {
    if !cfg.auto_upload {
        return None;
    }
    let targets = auto_upload_targets(cfg);
    if targets.is_empty() {
        return None;
    }
    Some((rec.id, sdroxide_types::qso_log_to_adif(std::slice::from_ref(rec)), targets))
}

/// Index of a spot kind into the app's `spot_kinds_shown` filter array.
fn spot_kind_index(kind: SpotKind) -> usize {
    match kind {
        SpotKind::DxCluster => 0,
        SpotKind::Pota => 1,
        SpotKind::Sota => 2,
        SpotKind::PskReporter => 3,
        SpotKind::FreeDv => 4,
    }
}

/// How the FT8/FT4 decode list orders the stations within each turn.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum DecodeSort {
    /// As received (no reordering).
    #[default]
    None,
    /// Strongest signal first.
    Signal,
    /// Farthest (DX) first.
    Distance,
}

pub struct SdroxideApp {
    ctrl: Box<dyn RadioController>,
    caps: Option<DeviceCaps>,
    state: RadioState,
    /// Latest spectrum frame, shared with the GPU waterfall callback — the Arc
    /// makes the per-repaint handoff a refcount bump instead of a bins clone.
    frame: Option<std::sync::Arc<SpectrumFrame>>,
    meters: Option<Meters>,
    memories: Vec<MemoryChannel>,
    view: ViewState,
    peaks: spectrum_view::PeakHold,
    /// UI-side smoothing for the spectrum *line* (waterfall stays un-averaged).
    spec_smooth: spectrum_view::SpectrumSmooth,
    error: Option<String>,
    /// Persistent, non-fatal operator notice (e.g. radio audio input
    /// unavailable / mono card selected for IQ). Shown as a warning banner.
    radio_notice: Option<String>,
    sent_cfg: Option<SpectrumConfig>,
    desired_cfg: Option<SpectrumConfig>,
    desired_at: f64,
    /// egui time of the last received spectrum frame, for stall detection.
    last_spectrum_at: f64,
    /// Waterfall time-scroll state: wall-clock (UTC secs) of the last tick and
    /// the carried fractional row, so the scroll rate is exact and independent
    /// of the frame rate (keeps the waterfall and time gridlines in lockstep).
    wf_last_now: f64,
    wf_row_accum: f32,
    /// Cached spectrum polylines (recomputed only when frame/view/rect change).
    trace_cache: spectrum_view::TraceCache,
    /// Switchable sound devices, queried once each time the settings dialog
    /// opens (cpal enumeration is too slow for per-frame).
    audio_devices: Option<AudioDevices>,
    audio_devices_queried: bool,
    /// Whether this build can drive SoapySDR (offered as an interface option).
    soapy_supported: bool,
    /// Settings dialog: current tab, plus the radio-backend config + serial
    /// ports loaded once on open (edited live, persisted on change).
    ///
    /// The tab is deliberately session-only — reopening the dialog returns to
    /// wherever you last were, but a restart starts again at General, so it is
    /// never written to storage in [`eframe::App::save`].
    settings_tab: SettingsTab,
    /// Display preferences (frame rate, waterfall + spectrum speed), loaded from
    /// config at startup, edited in the UI tab, persisted on change.
    ui_settings: sdroxide_types::UiSettings,
    radio_cfg: Option<sdroxide_types::RadioConfig>,
    serial_ports: Vec<String>,
    /// HPSDR devices found by the last "Discover" scan in the settings dialog.
    hpsdr_devices: Vec<sdroxide_types::HpsdrDevice>,
    /// FlexRadios found by the last discovery listen on the Radio tab.
    flex_devices: Vec<sdroxide_types::FlexDevice>,
    /// Result of the last TCI "Test connection" (Ok summary / Err message).
    tci_test_result: Option<Result<String, String>>,
    /// Result of the last FlexRadio "Test connection".
    flex_test_result: Option<Result<String, String>>,
    seen_first_state: bool,
    show_memories: bool,
    show_settings: bool,
    /// Voice keyer: the engine's slot list and what it is doing, the window's
    /// open state, and the one slot label being typed into (only the focused
    /// row is UI-owned, so the status echo can't fight the keyboard).
    voice: sdroxide_types::VoiceStatus,
    show_voice: bool,
    voice_name_edit: Option<(usize, String)>,
    /// When the band/mode, FFT and skimmer popups opened (egui time), for their
    /// auto-fade.
    mode_popup_since: Option<f64>,
    fft_popup_since: Option<f64>,
    skimmer_popup_since: Option<f64>,
    mem_name: String,
    // Skimmer (CW etc.) spots, newest merge-by-id.
    skimmer_spots: Vec<SkimmerSpot>,
    /// Per-spot last-active timestamp (egui seconds), so a box fades out over
    /// `SKIMMER_FADE_SECS` once its signal stops keying instead of vanishing.
    skimmer_active_at: std::collections::HashMap<u64, f64>,
    // FT8/FT4 digital-mode state.
    digi_decodes: Vec<Decode>,
    digi_status: Option<DigiStatus>,
    /// PSK/RTTY outgoing text buffer (UI-owned; streamed to the engine, which
    /// reports back how many characters have been sent so we colour them green).
    text_tx: String,
    qso_log: Vec<QsoRecord>,
    show_digi_settings: bool,
    /// UI-owned editable copy of the operator config, so typing isn't fought
    /// by the round-tripped status echo. Seeded once from the first status.
    digi_cfg_edit: sdroxide_types::DigiConfig,
    digi_cfg_seeded: bool,
    /// SSTV image-mode panel state (gallery, TX slots, message, textures).
    sstv: SstvUi,
    /// RF Paint (Spectrum Painting) panel state (text/image + previews).
    rf_paint: RfPaintUi,
    /// FSQ directed-message target callsign ("" = broadcast/ALLCALL).
    fsq_target: String,
    /// FSQ contacts (address book), native-persisted in `contacts.json`.
    fsq_contacts: Vec<sdroxide_types::FsqContact>,
    /// FSQ "add contact" input field.
    fsq_new_contact: String,
    /// Whether the FSQ contacts editor window is open.
    fsq_show_contacts: bool,
    /// FSQ received-image gallery (decoded textures, newest first).
    fsq_rx_images: Vec<egui::TextureHandle>,
    /// Picked-image inbox for FSQ image transmit (raw file bytes).
    fsq_img_inbox: std::sync::Arc<std::sync::Mutex<Option<Vec<u8>>>>,
    /// The last decode the user clicked (not REPLY): its call and map
    /// location, shown as a faint preview marker distinct from the active DX.
    digi_preview: Option<(String, (f64, f64))>,
    /// Animated centre/zoom of the FT8 world map (eased toward the fit target).
    map_view: crate::widgets::worldmap::MapView,
    /// Which decoded stations are currently up, and how brightly. Shared with
    /// the 3D globe so the flat map and the globe never disagree.
    digi_stations: crate::digi_map::DigiStations,
    /// Location of the decode row hovered this frame, shown on the map as a
    /// bright yellow dot. Frame-scoped (set by the decode list, read by the map).
    digi_hover_ll: Option<(f64, f64)>,
    /// Decode-list ordering within each turn, and whether to show CQ only.
    digi_sort: DecodeSort,
    /// Sort direction: `true` = descending (strongest / farthest first).
    digi_sort_desc: bool,
    digi_cq_only: bool,
    /// Decode-list filter: only stations that would put something new in the
    /// log (new entity, new band-slot, new grid, or a callsign never worked).
    digi_new_only: bool,
    /// The FT8 free-text entry, sent verbatim in the next transmit slot.
    digi_free_text: String,
    /// Voice-mode view span saved on entering FT8/FT4 (which locks the view to
    /// the narrow sub-band), restored on leaving so the panadapter isn't left
    /// stuck zoomed in.
    pre_digi_view: Option<(f64, f64)>,
    /// Logbook overlay open state, and the in-progress new/edit entry (if any).
    show_logbook: bool,
    log_edit: Option<LogEditForm>,
    // ── Network cockpit (spots / lookup / uploads) ──
    /// Latest merged network spots (DX cluster / POTA / SOTA / PSK Reporter).
    spots: Vec<Spot>,
    /// Latest feed/connection status line (cluster state, feed errors).
    net_status: Option<String>,
    /// Spots window open state.
    show_spots: bool,
    /// Which spot kinds are shown on the overlay/list (DX, POTA, SOTA, PSK,
    /// FREEDV) — indexed by [`spot_kind_index`].
    spot_kinds_shown: [bool; 5],
    /// Show only spots that fall inside the current panadapter view span.
    spot_in_view_only: bool,
    /// UI-owned editable copy of the network config (edited in the Settings
    /// dialog's Spots / FreeDV / Uploads tabs). Carries no operator identity —
    /// that comes from the digi config, edited on the General tab.
    net_cfg_edit: NetworkConfig,
    // ── Built-in TCI server ──
    /// UI-owned editable copy of the TCI server config, seeded from the
    /// controller when the settings dialog opens.
    tci_srv_edit: sdroxide_types::TciServerConfig,
    tci_srv_seeded: bool,
    /// Live server status (bound address, connected clients, bind error) from
    /// `RadioEvent::TciServerStatus`.
    tci_srv_status: Option<TciServerStatus>,
    // ── Built-in rigctld server ──
    /// UI-owned editable copy of the rigctld config, seeded from the controller
    /// when the settings dialog opens.
    rigctld_edit: sdroxide_types::RigctldConfig,
    rigctld_seeded: bool,
    // ── WSJT-X UDP broadcast (decodes / status / QSOs for the loggers) ──
    /// UI-owned editable copy, seeded from the engine like the configs above.
    wsjtx_edit: sdroxide_types::WsjtxConfig,
    wsjtx_seeded: bool,
    /// Live status from `RadioEvent::RigctldStatus`. Same shape as the TCI
    /// server's, so the two share one status type.
    rigctld_status: Option<TciServerStatus>,
    /// Editable "extra cluster commands" (one per line), split into
    /// `net_cfg_edit.cluster.commands` on apply.
    net_cluster_cmds: String,
    /// Rolling upload/lookup result log for the spots window (newest first).
    net_log: Vec<String>,
    /// Inbox for an ADIF file chosen via the native "Import" dialog (a picker
    /// thread writes; the UI drains it each frame).
    adif_import_inbox: Arc<Mutex<Option<String>>>,
    /// Callsigns queued for lookup, drained into commands each frame.
    pending_lookups: Vec<String>,
    /// QSO uploads queued (id, single-record ADIF, targets), drained to commands.
    pending_uploads: Vec<(u64, String, Vec<UploadTarget>)>,
    /// Awards dashboard open state + band filter ("" = all bands).
    show_awards: bool,
    awards_band: String,
    /// Cached award tally, keyed by (log length, band filter).
    awards_cache: Option<(usize, String, sdroxide_types::Awards)>,
    /// Cached set of worked DXCC entity names, keyed by log length (for the
    /// "new entity" spot badge).
    worked_entities_cache: Option<(usize, std::collections::HashSet<String>)>,
    /// Cached membership sets over the log, keyed by log length — the decode
    /// list asks these which stations would be a new one, every row, every slot.
    log_index_cache: Option<(usize, sdroxide_types::LogIndex)>,
    /// F1 help: the embedded user manual with a navigation outline.
    help: crate::help::Help,
    /// Control inputs: keyboard/mouse bindings, MIDI, and what is held right now.
    input: crate::input::InputRuntime,
    /// MIDI ports as `(id, name)`, enumerated when the settings dialog opens
    /// (touching the host MIDI stack is too slow for per-frame).
    midi_in_ports: Vec<(String, String)>,
    midi_out_ports: Vec<(String, String)>,
    /// Solar-system 3D view, shown in its own OS window (native-only).
    #[cfg(not(target_arch = "wasm32"))]
    solar: crate::solar3d::Solar3d,
}

/// Editable text fields for a manual logbook entry (new or edit). Kept as
/// strings so partial input doesn't fight the user; parsed on save. On edit,
/// `base` holds the original record so fields not shown in the form (QSL flags,
/// resolved DXCC/zones, …) survive a save.
#[derive(Default)]
struct LogEditForm {
    /// 0 = new entry; otherwise the id of the record being edited.
    id: u64,
    /// Timestamp fallback if the date/time fields don't parse.
    seed_utc: i64,
    /// Original record (edit) or default (new); preserves untouched fields.
    base: QsoRecord,
    call: String,
    grid: String,
    freq_mhz: String,
    mode: String,
    rst_sent: String,
    rst_rcvd: String,
    date: String,
    time: String,
    name: String,
    qth: String,
    state: String,
    country: String,
    tx_pwr: String,
    contest_id: String,
    srx: String,
    stx: String,
    comment: String,
}

impl LogEditForm {
    /// A blank new entry seeded with the current time, band, and mode.
    fn new_entry(now: i64, freq_hz: f64, mode: &str) -> LogEditForm {
        let (y, mo, d, h, mi, _) = sdroxide_types::utc_ymd_hms(now);
        LogEditForm {
            id: 0,
            seed_utc: now,
            freq_mhz: if freq_hz > 0.0 { format!("{:.4}", freq_hz / 1e6) } else { String::new() },
            mode: mode.to_string(),
            date: format!("{y:04}-{mo:02}-{d:02}"),
            time: format!("{h:02}:{mi:02}"),
            ..Default::default()
        }
    }

    fn from_record(r: &QsoRecord) -> LogEditForm {
        let (y, mo, d, h, mi, _) = sdroxide_types::utc_ymd_hms(r.start_utc);
        LogEditForm {
            id: r.id,
            seed_utc: r.start_utc,
            base: r.clone(),
            call: r.call.clone(),
            grid: r.grid.clone().unwrap_or_default(),
            freq_mhz: if r.freq_hz > 0.0 { format!("{:.4}", r.freq_hz / 1e6) } else { String::new() },
            mode: r.mode.clone(),
            rst_sent: r.rst_sent.map(|v| v.to_string()).unwrap_or_default(),
            rst_rcvd: r.rst_rcvd.map(|v| v.to_string()).unwrap_or_default(),
            date: format!("{y:04}-{mo:02}-{d:02}"),
            time: format!("{h:02}:{mi:02}"),
            name: r.name.clone(),
            qth: r.qth.clone(),
            state: r.state.clone(),
            country: r.country.clone(),
            tx_pwr: r.tx_pwr.map(|v| v.to_string()).unwrap_or_default(),
            contest_id: r.contest_id.clone(),
            srx: r.srx.map(|v| v.to_string()).unwrap_or_default(),
            stx: r.stx.map(|v| v.to_string()).unwrap_or_default(),
            comment: r.comment.clone(),
        }
    }

    /// Parse into a record, or `None` if the callsign is empty. Starts from
    /// `base` so unshown fields are preserved across an edit.
    fn to_record(&self, my_call: &str, my_grid: &str) -> Option<QsoRecord> {
        let call = self.call.trim().to_uppercase();
        if call.is_empty() {
            return None;
        }
        let freq_hz = self.freq_mhz.trim().parse::<f64>().ok().map(|m| m * 1e6).unwrap_or(0.0);
        let band =
            if freq_hz > 0.0 { sdroxide_types::adif_band(freq_hz).to_string() } else { String::new() };
        let start = parse_utc(&self.date, &self.time, self.seed_utc);
        let mode = {
            let m = self.mode.trim().to_uppercase();
            if m.is_empty() { "SSB".into() } else { m }
        };
        let mut rec = self.base.clone();
        rec.id = self.id;
        rec.call = call;
        rec.grid = {
            let g = self.grid.trim();
            (!g.is_empty()).then(|| g.to_uppercase())
        };
        rec.rst_sent = self.rst_sent.trim().parse().ok();
        rec.rst_rcvd = self.rst_rcvd.trim().parse().ok();
        rec.freq_hz = freq_hz;
        rec.mode = mode;
        rec.band = band;
        rec.start_utc = start;
        rec.end_utc = if rec.end_utc > start { rec.end_utc } else { start };
        rec.my_call = my_call.to_string();
        rec.my_grid = my_grid.to_string();
        rec.name = self.name.trim().to_string();
        rec.qth = self.qth.trim().to_string();
        rec.state = self.state.trim().to_uppercase();
        rec.country = self.country.trim().to_string();
        rec.tx_pwr = self.tx_pwr.trim().parse().ok();
        rec.contest_id = self.contest_id.trim().to_uppercase();
        rec.srx = self.srx.trim().parse().ok();
        rec.stx = self.stx.trim().parse().ok();
        rec.comment = self.comment.trim().to_string();
        Some(rec)
    }
}

impl SdroxideApp {
    pub fn new(cc: &eframe::CreationContext<'_>, ctrl: Box<dyn RadioController>) -> Self {
        crate::theme::apply(&cc.egui_ctx);
        if let Some(rs) = &cc.wgpu_render_state {
            waterfall_gpu::init(rs);
        }
        let view: ViewState = cc
            .storage
            .and_then(|s| eframe::get_value(s, "view"))
            .unwrap_or_default();
        // Copied out before `view` is moved into the struct below.
        #[cfg(not(target_arch = "wasm32"))]
        let solar3d_view = view.solar3d;
        let soapy_supported = ctrl.soapy_supported();
        // Seed the network config from disk up front so auto-lookup / auto-upload
        // are honoured from launch (not only after the setup window is opened).
        let net_cfg = ctrl.network_config().unwrap_or_default();
        let net_cluster_cmds = net_cfg.cluster.commands.join("\n");
        SdroxideApp {
            ctrl,
            caps: None,
            state: RadioState::default(),
            frame: None,
            meters: None,
            memories: Vec::new(),
            view,
            peaks: spectrum_view::PeakHold::default(),
            spec_smooth: spectrum_view::SpectrumSmooth::default(),
            error: None,
            radio_notice: None,
            sent_cfg: None,
            desired_cfg: None,
            desired_at: 0.0,
            last_spectrum_at: 0.0,
            wf_last_now: 0.0,
            wf_row_accum: 0.0,
            trace_cache: spectrum_view::TraceCache::default(),
            audio_devices: None,
            audio_devices_queried: false,
            soapy_supported,
            settings_tab: SettingsTab::General,
            ui_settings: load_ui_settings(cc.storage),
            radio_cfg: None,
            serial_ports: Vec::new(),
            hpsdr_devices: Vec::new(),
            flex_devices: Vec::new(),
            tci_test_result: None,
            flex_test_result: None,
            seen_first_state: false,
            show_memories: false,
            show_settings: false,
            voice: sdroxide_types::VoiceStatus::default(),
            show_voice: false,
            voice_name_edit: None,
            mode_popup_since: None,
            fft_popup_since: None,
            skimmer_popup_since: None,
            mem_name: String::new(),
            skimmer_spots: Vec::new(),
            skimmer_active_at: std::collections::HashMap::new(),
            digi_decodes: Vec::new(),
            digi_status: None,
            text_tx: String::new(),
            qso_log: load_qso_log(cc.storage),
            show_digi_settings: false,
            digi_cfg_edit: sdroxide_types::DigiConfig::default(),
            sstv: SstvUi::default(),
            rf_paint: RfPaintUi::default(),
            fsq_target: String::new(),
            fsq_contacts: fsq_load_contacts(),
            fsq_new_contact: String::new(),
            fsq_show_contacts: false,
            fsq_rx_images: Vec::new(),
            fsq_img_inbox: std::sync::Arc::new(std::sync::Mutex::new(None)),
            digi_cfg_seeded: false,
            digi_preview: None,
            map_view: Default::default(),
            digi_stations: Default::default(),
            digi_hover_ll: None,
            digi_sort: DecodeSort::None,
            digi_sort_desc: true,
            digi_cq_only: false,
            digi_new_only: false,
            digi_free_text: String::new(),
            pre_digi_view: None,
            show_logbook: false,
            log_edit: None,
            spots: Vec::new(),
            net_status: None,
            show_spots: false,
            spot_kinds_shown: [true; 5],
            spot_in_view_only: false,
            net_cfg_edit: net_cfg,
            rigctld_edit: sdroxide_types::RigctldConfig::default(),
            rigctld_seeded: false,
            wsjtx_edit: sdroxide_types::WsjtxConfig::default(),
            wsjtx_seeded: false,
            rigctld_status: None,
            tci_srv_edit: sdroxide_types::TciServerConfig::default(),
            tci_srv_seeded: false,
            tci_srv_status: None,
            net_cluster_cmds,
            net_log: Vec::new(),
            adif_import_inbox: Arc::new(Mutex::new(None)),
            pending_lookups: Vec::new(),
            pending_uploads: Vec::new(),
            show_awards: false,
            awards_band: String::new(),
            awards_cache: None,
            worked_entities_cache: None,
            log_index_cache: None,
            help: crate::help::Help::default(),
            input: crate::input::InputRuntime::new(cc.storage, &cc.egui_ctx),
            midi_in_ports: Vec::new(),
            midi_out_ports: Vec::new(),
            // The GPU resources are built on first open, not here: most
            // sessions never open this window.
            #[cfg(not(target_arch = "wasm32"))]
            solar: crate::solar3d::Solar3d::new(cc.wgpu_render_state.clone(), solar3d_view),
        }
    }

    /// The FT8/FT4 activity to plot on the 3D globe: the same decoded stations
    /// the flat map shows, plus the station being worked.
    ///
    /// Read-only — the decode bookkeeping stays in `qso_area`, which is the one
    /// place that knows a decode is new. Outside the digital modes the map
    /// simply ages out and empties.
    #[cfg(not(target_arch = "wasm32"))]
    fn digi_traffic(&self, now_t: f64) -> crate::solar3d::DigiTraffic {
        let status = self.digi_status.as_ref();
        self.digi_stations.traffic(
            now_t,
            status.and_then(|s| s.dx_grid.as_deref()),
            self.digi_preview.as_ref().map(|(_, ll)| *ll),
            status.is_some_and(|s| s.transmitting),
        )
    }

    /// The operator's grid square. Prefers the engine's copy but falls back to
    /// the UI's edit buffer: `digi_status` only arrives once the engine sends
    /// its first `DigiStatus`, and never at all in sessions with no digi engine.
    #[cfg(not(target_arch = "wasm32"))]
    fn my_grid(&self) -> String {
        self.digi_status
            .as_ref()
            .map(|s| s.config.my_grid.clone())
            .filter(|g| !g.is_empty())
            .unwrap_or_else(|| self.digi_cfg_edit.my_grid.clone())
    }

    /// Next free logbook id.
    fn next_log_id(&self) -> u64 {
        self.qso_log.iter().map(|q| q.id).max().unwrap_or(0) + 1
    }

    /// Desired engine-side spectrum config. The requested viewport gets 2×
    /// slack around the visible span so panning inside it needs no
    /// reconfiguration (which would clear the waterfall history); the FFT
    /// grows with zoom for real resolution.
    fn desired_spectrum_cfg(&self) -> SpectrumConfig {
        let full_span = self.state.sample_rate;
        let dev_lo = self.state.center_hz - full_span / 2.0;
        let dev_hi = self.state.center_hz + full_span / 2.0;
        let (viewport, zoom) = if !self.view.is_unset() && full_span > 0.0 {
            let vspan = self.view.span();
            let ratio = (full_span / vspan).max(1.0);
            if ratio > 1.05 {
                let slack = (vspan * 2.0).min(full_span);
                let center = (self.view.view_lo_hz + self.view.view_hi_hz) / 2.0;
                let lo = (center - slack / 2.0).clamp(dev_lo, dev_hi - slack);
                (Some((lo, lo + slack)), ratio)
            } else {
                (None, 1.0)
            }
        } else {
            (None, 1.0)
        };
        let mut fft = self.view.fft_size.max(1024);
        while (fft as f64) < self.view.fft_size as f64 * zoom.min(8.0) && fft < 32_768 {
            fft *= 2;
        }
        SpectrumConfig {
            fft_size: fft,
            db_floor: self.view.db_floor,
            db_ceil: self.view.db_ceil,
            viewport,
            // Frame rate comes from the UI settings and also drives the repaint
            // cadence (see the end of `ui`). Engine averaging is disabled so the
            // waterfall gets full detail; the spectrum *line* is smoothed UI-side
            // per the spectrum-speed setting (decoupled from the waterfall).
            fps: self.ui_settings.fps().min(255) as u8,
            avg_tc: 0.0,
        }
    }

    /// Advance the waterfall time-scroll one frame: convert the wall-clock
    /// elapsed since the last tick into a whole number of rows to append (at the
    /// configured rows/second), carrying the fraction. Returns the tuning the
    /// widget needs; the same rows/second also spaces the time gridlines, so the
    /// line and the waterfall move together. `has_frame` gates scrolling so a
    /// stalled stream doesn't keep duplicating rows.
    fn wf_tick(&mut self, has_frame: bool) -> spectrum_view::WfTuning {
        let now = now_unix_f64();
        let rows_per_sec = self.ui_settings.waterfall_rows_per_sec();
        // Clamp dt so a hitch/tab-away can't dump a huge run of rows at once.
        let dt = if self.wf_last_now > 0.0 { (now - self.wf_last_now).clamp(0.0, 0.3) } else { 0.0 };
        self.wf_last_now = now;
        let rows_to_write = if has_frame {
            self.wf_row_accum += dt as f32 * rows_per_sec;
            let n = self.wf_row_accum.floor();
            self.wf_row_accum -= n;
            (n as u32).min(32)
        } else {
            0
        };
        // Spectrum-line smoothing: convert the time constant to a per-frame EMA
        // coefficient using the frame rate, so the reaction time is the same at
        // any fps (0 tc = no smoothing = raw frames).
        let tc = self.ui_settings.spectrum_avg_tc();
        let fps = self.ui_settings.fps().max(1) as f32;
        let spectrum_alpha = if tc <= 0.0 { 1.0 } else { 1.0 - (-(1.0 / fps) / tc).exp() };
        let s = &self.ui_settings;
        let gradient = s.spectrum_gradient.then(|| {
            let [tr, tg, tb] = s.gradient_top;
            let [br, bg, bb] = s.gradient_bottom;
            (Color32::from_rgb(tr, tg, tb), Color32::from_rgb(br, bg, bb))
        });
        spectrum_view::WfTuning {
            rows_to_write,
            rows_per_sec,
            now_unix: now,
            spectrum_alpha,
            palette: s.waterfall_palette,
            gradient,
        }
    }

    /// Hysteresis: is the config the engine already has still fine for the
    /// current view? (Avoids waterfall-clearing resends while panning.)
    fn cfg_still_good(&self) -> bool {
        let Some(sent) = self.sent_cfg else { return false };
        let ideal = self.desired_spectrum_cfg();
        if sent.fft_size != ideal.fft_size
            || sent.db_floor != ideal.db_floor
            || sent.db_ceil != ideal.db_ceil
            || sent.fps != ideal.fps
            || sent.avg_tc != ideal.avg_tc
        {
            return false;
        }
        match (sent.viewport, ideal.viewport) {
            (None, None) => true,
            (Some((slo, shi)), Some(_)) => {
                let full_span = self.state.sample_rate;
                let dev_lo = self.state.center_hz - full_span / 2.0;
                let dev_hi = self.state.center_hz + full_span / 2.0;
                let sspan = shi - slo;
                let margin = sspan * 0.05;
                // Inside with margin, unless the sent window is pinned to a
                // device edge on that side.
                let lo_ok = self.view.view_lo_hz >= slo + margin || slo <= dev_lo + 1.0;
                let hi_ok = self.view.view_hi_hz <= shi - margin || shi >= dev_hi - 1.0;
                let res = sspan / self.view.span().max(1.0);
                lo_ok && hi_ok && (1.15..=3.5).contains(&res)
            }
            _ => false,
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui, cmds: &mut Vec<Command>) {
        ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
        // All controls are captioned (or bare) modules that reflow when the
        // window is narrow. The frequency box is always first, the S-meter
        // second; the rest follow and wrap to further rows.
        ui.with_layout(
            egui::Layout::left_to_right(egui::Align::Min).with_main_wrap(true),
            |ui| {
                self.freq_module(ui, cmds);
                self.smeter_module(ui);
                self.vfo_rit_module(ui, cmds);
                self.rx_filter_module(ui, cmds);
                if self.caps.as_ref().is_some_and(|c| c.is_transmit_capable()) {
                    self.tx_module(ui, cmds);
                }
                self.display_module(ui, cmds);
                self.windows_module(ui);
            },
        );
    }

    /// The VFO frequency controls (A/B select + big readout + the inactive
    /// VFO's frequency) in a label-less box, always the first module.
    fn freq_module(&mut self, ui: &mut egui::Ui, cmds: &mut Vec<Command>) {
        // The 10-digit readout is fixed width, so measure it (via the same fonts
        // freq_display uses) and size the box to hug its contents — that keeps the
        // right column against the box edge (no empty space) and lets the readout
        // be centred vertically by exact geometry rather than a fragile layout hint.
        let font40 = egui::FontId::monospace(40.0);
        let digit = ui
            .painter()
            .layout_no_wrap("0".to_owned(), font40.clone(), Color32::WHITE)
            .size();
        let dot_w = ui
            .painter()
            .layout_no_wrap(".".to_owned(), font40, Color32::WHITE)
            .size()
            .x;
        let hz_w = ui
            .painter()
            .layout_no_wrap(" Hz".to_owned(), egui::FontId::proportional(12.0), Color32::WHITE)
            .size()
            .x;
        // 10 digits + 3 group separators + " Hz", with freq_display's 1px spacing.
        let readout_w = 10.0 * digit.x + 3.0 * dot_w + hz_w + 13.0;
        let readout_h = digit.y;

        let ab_w = 68.0;
        let right_w = 96.0;
        let box_w = 8.0 + ab_w + 10.0 + readout_w + 12.0 + right_w + 8.0;

        crate::chrome::module_bare_h(ui, box_w, crate::chrome::MODULE_TALL_H, |ui| {
            ui.spacing_mut().item_spacing.x = 0.0; // control every gap explicitly
            let active = self.state.active_vfo;
            let full_h = ui.available_height();

            // VFO A/B selector, vertically centred in the full box height.
            let mut sel = None;
            ui.allocate_ui_with_layout(
                egui::vec2(ab_w, full_h),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    for (v, label) in [(Vfo::A, "A"), (Vfo::B, "B")] {
                        if crate::chrome::chip(ui, active == v, RichText::new(label).size(15.0))
                            .clicked()
                        {
                            sel = Some(v);
                        }
                    }
                },
            );
            if let Some(v) = sel {
                cmds.push(Command::SelectVfo(v));
            }
            ui.add_space(10.0);

            // Big frequency readout, centred vertically by measured height.
            let mut new_hz = None;
            ui.allocate_ui_with_layout(
                egui::vec2(readout_w, full_h),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.add_space(((full_h - readout_h) / 2.0).max(0.0));
                    new_hz = freq_display::show(
                        ui,
                        egui::Id::new("main-freq"),
                        self.state.active_freq_hz(),
                        self.input.cfg.wheel,
                    );
                },
            );
            if let Some(hz) = new_hz {
                cmds.push(Command::SetVfo { vfo: active, hz });
            }
            ui.add_space(12.0);

            // Right column: inactive VFO frequency anchored top-right, band/mode
            // selector anchored bottom-right, hard against the box edge.
            let inactive_hz = match active {
                Vfo::A => self.state.vfo_b_hz,
                Vfo::B => self.state.vfo_a_hz,
            };
            ui.allocate_ui_with_layout(
                egui::vec2(right_w, full_h),
                egui::Layout::top_down(egui::Align::Max),
                |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.label(
                        RichText::new(format!("{:.6} MHz", inactive_hz / 1e6))
                            .monospace()
                            .size(12.0)
                            .color(Color32::from_gray(120)),
                    );
                    let pad = (ui.available_height() - 24.0).max(0.0);
                    ui.add_space(pad);
                    self.band_mode_button(ui, cmds);
                },
            );
        });
    }

    /// The S-meter in a label-less box, always pinned top-right. Clicking it
    /// toggles between the bar and analog-needle styles.
    fn smeter_module(&mut self, ui: &mut egui::Ui) {
        crate::chrome::module_bare_flush_h(ui, 250.0, crate::chrome::MODULE_TALL_H, |ui| {
            let resp = smeter::show(ui, self.meters.as_ref(), self.view.smeter_analog)
                .on_hover_text("Click to switch bar / analog meter");
            if resp.clicked() {
                self.view.smeter_analog = !self.view.smeter_analog;
            }
        });
    }

    /// The CW-skimmer overlay: the current spots plus a parallel per-spot
    /// opacity that fades a box out over `SKIMMER_FADE_SECS` once it stops
    /// keying. Fully-faded spots are dropped so they free their lane.
    fn cw_overlay(&self, now: f64) -> (Vec<SkimmerSpot>, Vec<f32>) {
        let mut spots = Vec::new();
        let mut alpha = Vec::new();
        for s in &self.skimmer_spots {
            let a = if s.active {
                1.0
            } else {
                let last = self.skimmer_active_at.get(&s.id).copied().unwrap_or(now);
                (1.0 - (now - last) / SKIMMER_FADE_SECS).clamp(0.0, 1.0) as f32
            };
            if a <= 0.02 {
                continue;
            }
            spots.push(s.clone());
            alpha.push(a);
        }
        (spots, alpha)
    }

    /// Drop everything derived from the previous mode's decodes: the waterfall
    /// callsign boxes, the decode list, the world-map station dots and the
    /// clicked-decode preview. Called on every RX mode change so leaving FT8/FT4
    /// (for SSTV, a keyboard mode, or plain SSB) doesn't carry its labels over.
    fn clear_digi_rx(&mut self) {
        self.digi_decodes.clear();
        self.digi_stations = Default::default();
        self.digi_preview = None;
    }

    /// Reuse the skimmer overlay to mark FT8/FT4 stations: one box per decoded
    /// callsign at its audio frequency (`dial + audio_hz`). The newest slot is
    /// solid; the previous slot is dimmed. Clicking a box sets the audio offset.
    fn ft8_overlay(&self) -> (Vec<SkimmerSpot>, Vec<f32>) {
        let mut spots = Vec::new();
        let mut alpha = Vec::new();
        let Some(latest) = self.digi_decodes.first().map(|d| d.slot_utc) else {
            return (spots, alpha);
        };
        // Age the whole overlay against the wall clock, not just against its own
        // newest entry: once decoding stops the boxes expire instead of staying
        // on the waterfall indefinitely.
        if now_unix() - latest > FT8_LABEL_MAX_AGE_SECS {
            return (spots, alpha);
        }
        let dial = self.state.rx_freq_hz();
        let mut seen = std::collections::HashSet::new();
        for d in &self.digi_decodes {
            // Decodes are newest-first; show only the last couple of slots.
            if latest - d.slot_utc > 30 {
                break;
            }
            let Some(call) = &d.from else { continue };
            if !seen.insert(call.clone()) {
                continue; // keep the most recent decode per callsign
            }
            let newest = d.slot_utc == latest;
            spots.push(SkimmerSpot {
                id: hash_call(call),
                kind: SkimmerKind::Cw,
                freq_hz: dial + d.audio_hz as f64,
                callsign: Some(call.clone()),
                text: d.message.clone(),
                snr_db: d.snr_db,
                wpm: 0,
                active: newest,
            });
            alpha.push(if newest { 1.0 } else { 0.5 });
        }
        (spots, alpha)
    }

    /// The network-spot overlay: the currently-shown spots (filtered by kind and,
    /// optionally, to the panadapter view span) plus a parallel age-fade alpha.
    /// Newest spots are solid; they dim over the last quarter of their lifetime.
    fn net_overlay(&self, now_utc: i64) -> (Vec<Spot>, Vec<f32>) {
        let max_age = self.net_cfg_edit.spot_max_age_secs.max(60) as i64;
        let (lo, hi) = (self.view.view_lo_hz, self.view.view_hi_hz);
        let mut spots = Vec::new();
        let mut alpha = Vec::new();
        for s in &self.spots {
            if !self.spot_kinds_shown[spot_kind_index(s.kind)] {
                continue;
            }
            if self.spot_in_view_only && !(lo..=hi).contains(&s.freq_hz) {
                continue;
            }
            let age = (now_utc - s.when_utc).max(0);
            let a = if age > max_age {
                continue;
            } else if age as f64 > max_age as f64 * 0.75 {
                (1.0 - (age as f64 - max_age as f64 * 0.75) / (max_age as f64 * 0.25)) as f32
            } else {
                1.0
            };
            spots.push(s.clone());
            alpha.push(a.clamp(0.15, 1.0));
        }
        (spots, alpha)
    }

    /// Open a fresh log entry pre-filled from a clicked spot, and kick a
    /// callsign lookup if auto-lookup is on.
    fn prefill_from_spot(&mut self, spot: &Spot) {
        let mut form = LogEditForm::new_entry(now_unix(), spot.freq_hz, &spot.mode);
        form.call = spot.call.clone();
        if let Some(g) = &spot.grid {
            form.grid = g.clone();
        }
        if !spot.comment.is_empty() {
            form.comment = spot.comment.clone();
        }
        self.log_edit = Some(form);
        self.show_logbook = true;
        self.queue_lookup(spot.call.clone());
    }

    /// Queue an auto-lookup if a provider + auto-lookup are configured.
    fn queue_lookup(&mut self, call: String) {
        let call = call.trim().to_string();
        if call.is_empty()
            || !self.net_cfg_edit.auto_lookup
            || self.net_cfg_edit.lookup_provider == LookupProvider::None
        {
            return;
        }
        self.pending_lookups.push(call);
    }

    /// Merge a callsign-lookup result into the open log entry and, if none
    /// matches, into the most recent logged QSO for that call — filling only
    /// blank fields.
    fn apply_callsign(&mut self, info: CallsignInfo) {
        let mut summary = info.call.clone();
        if let Some(n) = &info.name {
            summary.push_str(&format!(" — {n}"));
        }
        if let Some(q) = &info.qth {
            summary.push_str(&format!(", {q}"));
        }
        if let Some(c) = &info.country {
            summary.push_str(&format!(" ({c})"));
        }
        self.push_net_log(summary);

        // 1) The open entry form, if it's for this call.
        if let Some(f) = self.log_edit.as_mut() {
            if f.call.trim().eq_ignore_ascii_case(&info.call) {
                fill_blank(&mut f.name, info.name.as_deref());
                fill_blank(&mut f.qth, info.qth.as_deref());
                fill_blank(&mut f.grid, info.grid.as_deref());
                fill_blank(&mut f.state, info.state.as_deref());
                fill_blank(&mut f.country, info.country.as_deref());
                return;
            }
        }
        // 2) Otherwise, enrich the most recent logged QSO with that call.
        if let Some(rec) = self
            .qso_log
            .iter_mut()
            .filter(|q| q.call.eq_ignore_ascii_case(&info.call))
            .max_by_key(|q| q.start_utc)
        {
            let mut changed = false;
            changed |= fill_blank(&mut rec.name, info.name.as_deref());
            changed |= fill_blank(&mut rec.qth, info.qth.as_deref());
            changed |= fill_blank(&mut rec.state, info.state.as_deref());
            changed |= fill_blank(&mut rec.country, info.country.as_deref());
            if rec.grid.as_deref().unwrap_or("").is_empty() {
                if let Some(g) = &info.grid {
                    rec.grid = Some(g.clone());
                    changed = true;
                }
            }
            if rec.dxcc.is_none() && info.dxcc.is_some() {
                rec.dxcc = info.dxcc;
                changed = true;
            }
            if rec.cq_zone.is_none() && info.cq_zone.is_some() {
                rec.cq_zone = info.cq_zone;
                changed = true;
            }
            if rec.itu_zone.is_none() && info.itu_zone.is_some() {
                rec.itu_zone = info.itu_zone;
                changed = true;
            }
            if changed {
                persist_qso_log(&self.qso_log);
            }
        }
    }

    /// Record an upload result and mark the QSO's sent flag on success.
    fn on_upload_result(&mut self, r: UploadResult) {
        let status = if r.ok { "OK" } else { "FAIL" };
        self.push_net_log(format!("{} → {}: {}", r.target.label(), status, r.message));
        if r.ok {
            if let Some(rec) = self.qso_log.iter_mut().find(|q| q.id == r.qso_id) {
                match r.target {
                    UploadTarget::Eqsl => rec.eqsl_sent = true,
                    UploadTarget::QrzLogbook => rec.qrz_sent = true,
                    UploadTarget::ClubLog => rec.clublog_sent = true,
                }
                persist_qso_log(&self.qso_log);
            }
        }
    }

    /// Match downloaded QSL confirmations against the log by call + band (and,
    /// when both have one, mode) within a day, and OR the confirmation flags
    /// onto the local record.
    fn apply_confirmations(&mut self, recs: Vec<QsoRecord>) {
        let mut matched = 0usize;
        let mut changed = false;
        for c in &recs {
            if c.call.trim().is_empty() {
                continue;
            }
            if let Some(local) = self.qso_log.iter_mut().find(|q| {
                q.call.eq_ignore_ascii_case(&c.call)
                    && q.band.eq_ignore_ascii_case(&c.band)
                    && (c.mode.is_empty() || q.mode.eq_ignore_ascii_case(&c.mode))
                    && (q.start_utc - c.start_utc).abs() < 86_400
            }) {
                let before = (local.lotw_rcvd, local.eqsl_rcvd, local.qsl_rcvd);
                local.lotw_rcvd |= c.lotw_rcvd;
                local.eqsl_rcvd |= c.eqsl_rcvd;
                local.qsl_rcvd |= c.qsl_rcvd;
                if before != (local.lotw_rcvd, local.eqsl_rcvd, local.qsl_rcvd) {
                    changed = true;
                    matched += 1;
                }
            }
        }
        if changed {
            persist_qso_log(&self.qso_log);
        }
        self.push_net_log(format!(
            "Confirmations: {} downloaded, {matched} newly confirmed",
            recs.len()
        ));
    }

    fn push_net_log(&mut self, line: String) {
        self.net_log.insert(0, line);
        self.net_log.truncate(50);
    }

    /// The set of worked DXCC entity names (cached; recomputed when the log
    /// length changes). Used to flag "new entity" spots.
    fn worked_entities(&mut self) -> &std::collections::HashSet<String> {
        let len = self.qso_log.len();
        let stale = self.worked_entities_cache.as_ref().map(|(l, _)| *l != len).unwrap_or(true);
        if stale {
            let set: std::collections::HashSet<String> = self
                .qso_log
                .iter()
                .filter_map(|q| sdroxide_types::entity_name(&q.call).map(str::to_string))
                .collect();
            self.worked_entities_cache = Some((len, set));
        }
        &self.worked_entities_cache.as_ref().unwrap().1
    }

    /// Membership sets over the log (cached; rebuilt when the log length
    /// changes), so every decode row can be judged new-or-dupe for free.
    fn log_index(&mut self) -> &sdroxide_types::LogIndex {
        let len = self.qso_log.len();
        if self.log_index_cache.as_ref().map(|(l, _)| *l != len).unwrap_or(true) {
            self.log_index_cache = Some((len, sdroxide_types::LogIndex::build(&self.qso_log)));
        }
        &self.log_index_cache.as_ref().unwrap().1
    }

    /// The cached award tally for the current band filter (recomputed when the
    /// log length or the band filter changes).
    fn ensure_awards(&mut self) {
        let len = self.qso_log.len();
        let band = self.awards_band.clone();
        let stale = self
            .awards_cache
            .as_ref()
            .map(|(l, b, _)| *l != len || *b != band)
            .unwrap_or(true);
        if stale {
            let filter = (!band.is_empty()).then_some(band.as_str());
            let awards = sdroxide_types::compute_awards(&self.qso_log, filter, None);
            self.awards_cache = Some((len, band, awards));
        }
    }

    /// The awards dashboard: DXCC / WAS / WAZ / grid counts (worked vs
    /// confirmed) with a band filter, plus the WAS state grid and WAZ zone grid.
    fn awards_window(&mut self, ctx: &egui::Context) {
        if !self.show_awards {
            return;
        }
        self.ensure_awards();
        let bands = [
            "", "160m", "80m", "40m", "30m", "20m", "17m", "15m", "12m", "10m", "6m", "2m", "70cm",
            "23cm",
        ];
        let mut open = self.show_awards;
        let mut new_band: Option<String> = None;
        let awards = self.awards_cache.as_ref().map(|(_, _, a)| a.clone()).unwrap_or_default();
        let resp = egui::Window::new("AWARDS")
            .open(&mut open)
            .frame(crate::chrome::window_frame())
            .resizable(true)
            .default_width(540.0)
            .default_height(560.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Band").size(11.0).color(Color32::from_gray(150)));
                    for b in bands {
                        let label = if b.is_empty() { "All" } else { b };
                        if crate::chrome::chip(ui, self.awards_band == b, label).clicked() {
                            new_band = Some(b.to_string());
                        }
                    }
                });
                ui.separator();
                // Summary counts.
                award_summary(ui, "DXCC", &awards.dxcc);
                award_summary(ui, "WAZ", &awards.waz);
                award_summary(ui, "WAS", &awards.was);
                award_summary(ui, "Grids", &awards.grids);
                ui.add_space(6.0);

                egui::ScrollArea::vertical().auto_shrink([false, false]).show_themed(ui, |ui| {
                    // WAS state grid.
                    ui.label(RichText::new("Worked All States").size(12.0).strong().color(crate::theme::CYAN));
                    award_cell_grid(
                        ui,
                        sdroxide_types::US_STATES.iter().map(|s| {
                            (s.to_string(), awards.was.get(*s).copied().unwrap_or_default())
                        }),
                        44.0,
                    );
                    ui.add_space(8.0);
                    // WAZ zone grid (1..40).
                    ui.label(RichText::new("CQ Zones (WAZ)").size(12.0).strong().color(crate::theme::CYAN));
                    award_cell_grid(
                        ui,
                        (1u8..=40).map(|z| {
                            (format!("{z:02}"), awards.waz.get(&z).copied().unwrap_or_default())
                        }),
                        34.0,
                    );
                    ui.add_space(8.0);
                    // DXCC worked list (confirmed marked).
                    ui.label(RichText::new("DXCC entities").size(12.0).strong().color(crate::theme::CYAN));
                    for (name, st) in &awards.dxcc {
                        let col = if st.confirmed {
                            crate::theme::GREEN
                        } else {
                            crate::theme::YELLOW
                        };
                        ui.label(
                            RichText::new(format!("{} {name}", if st.confirmed { "✓" } else { "•" }))
                                .size(11.5)
                                .color(col),
                        );
                    }
                });
            });
        if let Some(r) = &resp {
            crate::chrome::paint_window_border(ctx, &r.response);
        }
        self.show_awards = open;
        if let Some(b) = new_band {
            self.awards_band = b;
        }
    }

    /// Drain a pending ADIF import: parse, lightly de-dup against the current
    /// log (same call+band within 2 minutes), append with fresh ids, persist.
    fn poll_adif_import(&mut self) {
        let text = self.adif_import_inbox.lock().ok().and_then(|mut g| g.take());
        let Some(text) = text else { return };
        let records = sdroxide_types::adif_to_qso_log(&text);
        let mut added = 0usize;
        let mut skipped = 0usize;
        for mut r in records {
            if r.call.trim().is_empty() {
                continue;
            }
            let dup = self.qso_log.iter().any(|q| {
                q.call.eq_ignore_ascii_case(&r.call)
                    && q.band.eq_ignore_ascii_case(&r.band)
                    && (q.start_utc - r.start_utc).abs() < 120
            });
            if dup {
                skipped += 1;
                continue;
            }
            r.id = self.next_log_id();
            self.qso_log.push(r);
            added += 1;
        }
        if added > 0 {
            persist_qso_log(&self.qso_log);
        }
        self.push_net_log(format!("ADIF import: {added} added, {skipped} duplicates skipped"));
    }

    /// Combined VFO + RIT/XIT box: the VFO A/B utility chips on top, with the
    /// RIT/XIT tuning-offset controls stacked underneath. Bare and tall — this
    /// replaces the separate VFO and RIT/XIT boxes.
    fn vfo_rit_module(&mut self, ui: &mut egui::Ui, cmds: &mut Vec<Command>) {
        let tx_capable = self.caps.as_ref().is_some_and(|c| c.is_transmit_capable());
        // Fixed field width, wide enough for a signed 4-digit offset plus " Hz".
        let hz_field = egui::vec2(74.0, 22.0);
        crate::chrome::module_bare_h(ui, 270.0, crate::chrome::MODULE_TALL_H, |ui| {
            ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(5.0, 5.0);
                // VFO utility chips.
                ui.horizontal(|ui| {
                    if crate::chrome::chip(ui, false, "A↔B").on_hover_text("Swap VFOs").clicked() {
                        cmds.push(Command::SwapVfos);
                    }
                    if crate::chrome::chip(ui, false, "A→B").on_hover_text("Copy A to B").clicked() {
                        cmds.push(Command::CopyAtoB);
                    }
                    if crate::chrome::chip(ui, self.state.split, "SPLIT").clicked() {
                        cmds.push(Command::SetSplit(!self.state.split));
                    }
                    if crate::chrome::chip(ui, self.state.sub_rx_enabled, "SUB")
                        .on_hover_text("Sub receiver on the inactive VFO (right ear)")
                        .clicked()
                    {
                        cmds.push(Command::SetSubRx(!self.state.sub_rx_enabled));
                    }
                });
                // RIT / XIT tuning offsets.
                ui.horizontal(|ui| {
                    let rit = self.state.rit;
                    if crate::chrome::chip(ui, rit.enabled, "RIT").clicked() {
                        cmds.push(Command::SetRit { enabled: !rit.enabled, hz: rit.hz });
                    }
                    let mut rit_hz = rit.hz;
                    if ui
                        .add_sized(
                            hz_field,
                            DragValue::new(&mut rit_hz).speed(5).range(-9999..=9999).suffix(" Hz"),
                        )
                        .changed()
                    {
                        cmds.push(Command::SetRit { enabled: rit.enabled, hz: rit_hz });
                    }
                    if tx_capable {
                        let xit = self.state.xit;
                        if crate::chrome::chip(ui, xit.enabled, "XIT").clicked() {
                            cmds.push(Command::SetXit { enabled: !xit.enabled, hz: xit.hz });
                        }
                        let mut xit_hz = xit.hz;
                        if ui
                            .add_sized(
                                hz_field,
                                DragValue::new(&mut xit_hz)
                                    .speed(5)
                                    .range(-9999..=9999)
                                    .suffix(" Hz"),
                            )
                            .changed()
                        {
                            cmds.push(Command::SetXit { enabled: xit.enabled, hz: xit_hz });
                        }
                    }
                });
            });
        });
    }

    /// The band/mode selector button plus the floating popup with the band +
    /// mode + digital button rows.
    fn band_mode_button(&mut self, ui: &mut egui::Ui, cmds: &mut Vec<Command>) {
        let mode = self.state.rx[0].mode;
        let summary = format!("{} · {}", self.state.band.label(), mode.label());
        let btn = crate::chrome::chip(ui, false, RichText::new(summary).size(14.0));

        let popup_id = egui::Popup::default_response_id(&btn);
        let now = ui.input(|i| i.time);
        let alpha =
            crate::chrome::popup_fade_alpha(ui.ctx(), popup_id, now, &mut self.mode_popup_since);
        let resp = egui::Popup::from_toggle_button_response(&btn)
                .frame(crate::chrome::window_frame_alpha(alpha))
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                .show(|ui| {
                    ui.set_opacity(alpha);
                    ui.set_max_width(430.0);
                    ui.label(RichText::new("BAND").color(crate::theme::CYAN_DIM).size(9.5).strong());
                    let digital = mode.is_digital();
                    ui.horizontal_wrapped(|ui| {
                        for b in Band::ALL {
                            // In a digital mode, a band button tunes to that
                            // band's FT8/FT4 dial frequency (SetVfo keeps the
                            // mode); otherwise it's a normal band change. Bands
                            // with no standard digital frequency are disabled.
                            // RF Paint has no calling frequency, so its band
                            // buttons jump to the band's default frequency while
                            // staying in RF Paint — every band the radio can
                            // reach is available.
                            let digi_hz = if mode.is_rf_paint() {
                                Some(b.default_entry().0)
                            } else if digital {
                                digi_freq_for_band(mode, b)
                            } else {
                                None
                            };
                            let cap_ok = self.caps.as_ref().is_none_or(|c| {
                                b.edges().is_none_or(|(lo, hi)| c.can_rx_hz(lo) || c.can_rx_hz(hi))
                            });
                            let enabled = cap_ok && (!digital || digi_hz.is_some());
                            let active = if mode.is_rf_paint() {
                                self.state.band == b
                            } else {
                                match digi_hz {
                                    Some(hz) => (self.state.active_freq_hz() - hz).abs() < 500.0,
                                    None => !digital && self.state.band == b,
                                }
                            };
                            let clicked = ui
                                .add_enabled_ui(enabled, |ui| {
                                    crate::chrome::chip(ui, active, b.label())
                                })
                                .inner
                                .clicked();
                            if clicked {
                                match digi_hz {
                                    Some(hz) => cmds.push(Command::SetVfo {
                                        vfo: self.state.active_vfo,
                                        hz,
                                    }),
                                    None => cmds.push(Command::SetBand(b)),
                                }
                            }
                        }
                    });
                    ui.add_space(6.0);
                    ui.label(RichText::new("MODE").color(crate::theme::CYAN_DIM).size(9.5).strong());
                    ui.horizontal_wrapped(|ui| {
                        for m in [Mode::Lsb, Mode::Usb, Mode::Cw, Mode::Am, Mode::Sam,
                                  Mode::Nfm, Mode::Wfm, Mode::Digu, Mode::Digl, Mode::Dsb, Mode::Spec] {
                            if crate::chrome::chip(ui, mode == m, m.label()).clicked() {
                                cmds.push(Command::SetMode { rx: RxId::Main, mode: m });
                            }
                        }
                    });
                    ui.add_space(6.0);
                    ui.label(RichText::new("DIGITAL").color(crate::theme::CYAN_DIM).size(9.5).strong());
                    ui.horizontal_wrapped(|ui| {
                        for m in Mode::DIGITAL {
                            if crate::chrome::chip(ui, mode == m, m.label()).clicked() {
                                cmds.push(Command::SetMode { rx: RxId::Main, mode: m });
                            }
                        }
                    });
                });
        if let Some(r) = &resp {
            crate::chrome::paint_popup_cut_border(ui.ctx(), &r.response, alpha);
            if r.response.contains_pointer() {
                self.mode_popup_since = Some(now); // keep it up while the pointer is on it
            }
        }
    }

    /// Combined Receiver + Filter/Noise box: AGC / volume / mute on top, with the
    /// squelch + noise-blanker + auto-notch + noise-reduction controls stacked
    /// underneath. Bare and tall, like the VFO/RIT box — replaces the separate
    /// Receiver and Filter boxes.
    fn rx_filter_module(&mut self, ui: &mut egui::Ui, cmds: &mut Vec<Command>) {
        crate::chrome::module_bare_h(ui, 356.0, crate::chrome::MODULE_TALL_H, |ui| {
            ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(5.0, 5.0);
                // Receiver: volume, AGC, mute.
                ui.horizontal(|ui| {
                    let mut vol = self.state.rx[0].volume;
                    ui.label("Vol");
                    if crate::chrome::slider(ui, Slider::new(&mut vol, 0.0..=1.0).show_value(false))
                        .changed()
                    {
                        self.state.rx[0].volume = vol; // optimistic echo
                        cmds.push(Command::SetVolume { rx: RxId::Main, v: vol });
                    }
                    let agc = self.state.rx[0].agc;
                    ComboBox::from_id_salt("agc")
                        .selected_text(format!("AGC {}", agc.label()))
                        .width(88.0)
                        .show_ui(ui, |ui| {
                            for a in AgcMode::ALL {
                                if ui.selectable_label(agc == a, a.label()).clicked() {
                                    cmds.push(Command::SetAgc { rx: RxId::Main, agc: a });
                                }
                            }
                        });
                    // AGC-T, next to the speed it belongs with. Only on the
                    // FlexRadio: its operators expect this control (SmartSDR has
                    // it), whereas a CAT rig's own AGC threshold sits in the
                    // radio and is not ours to move — showing the slider there
                    // would promise something it doesn't do.
                    if self.caps.as_ref().is_some_and(|c| c.driver == "flex")
                        && agc != AgcMode::Off
                    {
                        let mut db = self.state.rx[0].agc_max_gain_db;
                        ui.label("AGC-T").on_hover_text(
                            "How far the AGC may lift weak signals. Turn it down until the \
                             band noise stops being pumped up between signals.",
                        );
                        if crate::chrome::slider(
                            ui,
                            Slider::new(&mut db, 20.0..=120.0).step_by(1.0).suffix(" dB"),
                        )
                        .changed()
                        {
                            self.state.rx[0].agc_max_gain_db = db; // optimistic echo
                            cmds.push(Command::SetAgcMaxGain { rx: RxId::Main, db });
                        }
                    }
                    let muted = self.state.rx[0].muted;
                    if crate::chrome::chip_accent(ui, muted, "MUTE", crate::theme::PINK, Color32::WHITE)
                        .clicked()
                    {
                        cmds.push(Command::SetMute { rx: RxId::Main, muted: !muted });
                    }
                    // Record receiver audio to an MP3 file (toggling).
                    let recording = self.state.recording;
                    let rec = crate::chrome::chip_accent(
                        ui,
                        recording,
                        "REC",
                        crate::theme::PINK,
                        Color32::WHITE,
                    )
                    .on_hover_text(match &self.state.recording_file {
                        Some(f) => format!("Recording to {f} — click to stop"),
                        None => "Record receiver audio to MP3".to_string(),
                    });
                    if rec.clicked() {
                        cmds.push(Command::SetRecording(!recording));
                    }
                });
                // Filter / Noise: squelch, noise blanker.
                ui.horizontal(|ui| {
                    let mut sql = self.state.rx[0].squelch_db;
                    ui.label("SQL");
                    if crate::chrome::slider(
                        ui,
                        Slider::new(&mut sql, sdroxide_types::SQUELCH_OPEN_DB..=-30.0)
                            .show_value(true)
                            .custom_formatter(|v, _| {
                                if v <= (sdroxide_types::SQUELCH_OPEN_DB + 1.0) as f64 {
                                    "off".into()
                                } else {
                                    format!("{v:.0}")
                                }
                            }),
                    )
                    .changed()
                    {
                        self.state.rx[0].squelch_db = sql; // optimistic echo
                        cmds.push(Command::SetSquelch { rx: RxId::Main, db: sql });
                    }
                    let nb = self.state.noise_blanker;
                    if crate::chrome::chip(ui, nb, "NB")
                        .on_hover_text("Impulse noise blanker")
                        .clicked()
                    {
                        cmds.push(Command::SetNoiseBlanker(!nb));
                    }
                    // Auto-notch — cancels constant tones (heterodynes / carriers).
                    let anc = self.state.rx[0].auto_notch;
                    if crate::chrome::chip(ui, anc, "ANC")
                        .on_hover_text("Auto-notch: cancel constant tone elements (heterodynes)")
                        .clicked()
                    {
                        self.state.rx[0].auto_notch = !anc; // optimistic echo
                        cmds.push(Command::SetAutoNotch { rx: RxId::Main, on: !anc });
                    }
                    // Noise reduction — cycles Off → AI Low/Med/High (neural
                    // RNNoise) → NR Low/Mid/High (spectral) → Off.
                    let nr = self.state.rx[0].noise_reduction;
                    let nr_label =
                        if nr.is_on() { format!("NR {}", nr.label()) } else { "NR".to_string() };
                    if crate::chrome::chip(ui, nr.is_on(), nr_label)
                        .on_hover_text(
                            "Noise reduction (voice) — click to cycle: AI Low/Med/High (neural RNNoise), then NR Low/Mid/High (spectral), then Off",
                        )
                        .clicked()
                    {
                        let next = nr.next();
                        self.state.rx[0].noise_reduction = next; // optimistic echo
                        cmds.push(Command::SetNoiseReduction { rx: RxId::Main, level: next });
                    }
                });
            });
        });
    }

    fn tx_module(&mut self, ui: &mut egui::Ui, cmds: &mut Vec<Command>) {
        // The voice keyer's button only appears where the keyer can transmit —
        // every voice mode plus RADE, which takes a message as its microphone.
        let keyer_ok = self.state.rx[0].mode.allows_voice_keyer();
        // The ATU button and its readout only exist on a radio that has a
        // tuner to drive (the FlexRadio models fitted with one); the module
        // grows to make room rather than squeezing the rest.
        let atu_ok = self.caps.as_ref().is_some_and(|c| c.has_atu);
        let width = if keyer_ok { 520.0 } else { 470.0 } + if atu_ok { 130.0 } else { 0.0 };
        crate::chrome::module(ui, "Transmit", width, |ui| {
            let tx = self.state.tx;
            if crate::chrome::chip_accent(
                ui,
                tx.ptt,
                RichText::new(" PTT ").size(15.0).strong(),
                crate::theme::PINK,
                Color32::WHITE,
            )
            .clicked()
            {
                cmds.push(Command::SetPtt(!tx.ptt));
            }
            if crate::chrome::chip_accent(
                ui,
                tx.tune,
                RichText::new(" TUNE ").size(15.0),
                crate::theme::YELLOW,
                crate::theme::INK_ON_CYAN,
            )
            .clicked()
            {
                cmds.push(Command::SetTune(!tx.tune));
            }
            if atu_ok {
                use sdroxide_types::AtuState;
                let atu = self.state.atu;
                // Lit while a match is in circuit and while the cycle runs, so
                // the button is also the "tuner is doing something" indicator.
                let lit = atu.is_engaged() || atu == AtuState::Tuning;
                let hover = match atu {
                    AtuState::Success => "Tuner in circuit — click to bypass it",
                    AtuState::Tuning => "Tuning — the radio is transmitting",
                    AtuState::Failed => "The last tune failed — click to run it again",
                    _ => "Tune the radio's antenna tuner (this transmits briefly)",
                };
                let clicked = crate::chrome::chip_accent(
                    ui,
                    lit,
                    RichText::new(" ATU ").size(15.0),
                    crate::theme::YELLOW,
                    crate::theme::INK_ON_CYAN,
                )
                .on_hover_text(hover)
                .clicked();
                // A cycle takes a second or two and cannot be usefully
                // interrupted, so clicks during it are ignored rather than
                // starting a second tune.
                if clicked && atu != AtuState::Tuning {
                    cmds.push(if atu.is_engaged() {
                        Command::BypassAtu
                    } else {
                        Command::StartAtu
                    });
                }
                let colour = match atu {
                    AtuState::Success => Color32::from_rgb(90, 200, 110),
                    AtuState::Failed => Color32::from_rgb(230, 90, 80),
                    AtuState::Tuning => crate::theme::YELLOW,
                    _ => Color32::GRAY,
                };
                ui.label(RichText::new(atu.label()).color(colour).size(12.0));
            }
            if keyer_ok {
                // Lit while a message is on the air, so the button doubles as
                // the "something is transmitting from the keyer" indicator.
                let playing = self.voice.playing.is_some();
                let hover = match self.voice.playing {
                    Some(i) => format!(
                        "Transmitting {} — click to open the voice keyer",
                        sdroxide_types::slot_label(
                            i as usize,
                            &self.voice.slot(i as usize).name
                        )
                    ),
                    None => "Voice keyer: record and transmit stored messages".to_string(),
                };
                if crate::chrome::chip_accent(
                    ui,
                    playing || self.show_voice,
                    RichText::new(" ▶ ").size(15.0),
                    if playing { crate::theme::PINK } else { crate::theme::CYAN },
                    if playing { Color32::WHITE } else { crate::theme::INK_ON_CYAN },
                )
                .on_hover_text(hover)
                .clicked()
                {
                    self.show_voice = !self.show_voice;
                }
            }
            let mut drive = tx.drive;
            ui.label("Drive");
            if crate::chrome::slider(
                ui,
                Slider::new(&mut drive, 0.0..=1.0)
                    .show_value(true)
                    .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
            )
            .changed()
            {
                cmds.push(Command::SetTxDrive(drive));
            }
            let mut tune_drive = tx.tune_drive;
            ui.label("Tune");
            if crate::chrome::slider(
                ui,
                Slider::new(&mut tune_drive, 0.0..=1.0)
                    .show_value(true)
                    .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
            )
            .changed()
            {
                cmds.push(Command::SetTuneDrive(tune_drive));
            }
            let mut mic = tx.mic_gain;
            ui.label("Mic");
            if crate::chrome::slider(ui, Slider::new(&mut mic, 0.0..=1.0).show_value(false))
                .changed()
            {
                cmds.push(Command::SetMicGain(mic));
            }
        });
    }

    /// Auto-set floor/ceiling from the current frame for best waterfall
    /// contrast (noise dark, signals visible, no over-blow). Only the bins
    /// inside the visible viewport are considered, so signals scrolled or
    /// zoomed off-screen (e.g. a strong broadcaster) don't skew the levels —
    /// the emitted frame carries slack beyond the view.
    fn auto_levels(&mut self) {
        let result = {
            let Some(f) = self.frame.as_ref() else { return };
            let n = f.bins.len();
            if n == 0 || f.span_hz <= 0.0 {
                return;
            }
            let base = f.center_hz - f.span_hz / 2.0;
            let to_idx = |hz: f64| (hz - base) / f.span_hz * n as f64;
            let i_lo = (to_idx(self.view.view_lo_hz).floor().max(0.0) as usize).min(n);
            let i_hi = (to_idx(self.view.view_hi_hz).ceil().max(0.0) as usize).min(n);
            let slice = if i_hi > i_lo { &f.bins[i_lo..i_hi] } else { &f.bins[..] };
            pick_levels(slice, f.db_floor, f.db_ceil)
        };
        if let Some((floor, ceil)) = result {
            self.view.db_floor = floor;
            self.view.db_ceil = ceil;
        }
    }

    /// The SKIM chip: lit while any skimmer runs, and a popup with one row per
    /// kind (CW / PSK / RTTY) — an on/off chip plus that skimmer's squelch, the
    /// SNR a track must reach before it earns a box on the waterfall. Fades out
    /// on its own like the band/mode popup.
    fn skimmer_button(&mut self, ui: &mut egui::Ui, cmds: &mut Vec<Command>) {
        let btn = crate::chrome::chip(ui, self.state.skimmer.any_enabled(), "SKIM").on_hover_text(
            "CW / PSK / RTTY skimmers — decode signals across the band and mark them on the waterfall",
        );
        // A CAT rig feeding demodulated audio has no IQ span to skim; the engine
        // forces the skimmers off there, so the rows are shown disabled.
        let wideband = self.caps.as_ref().is_none_or(|c| !c.audio_mode);
        let popup_id = egui::Popup::default_response_id(&btn);
        let now = ui.input(|i| i.time);
        let alpha =
            crate::chrome::popup_fade_alpha(ui.ctx(), popup_id, now, &mut self.skimmer_popup_since);
        let resp = egui::Popup::from_toggle_button_response(&btn)
            .frame(crate::chrome::window_frame_alpha(alpha))
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
                ui.set_opacity(alpha);
                ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
                ui.label(RichText::new("SKIMMERS").color(crate::theme::CYAN_DIM).size(9.5).strong());
                // Edit a copy and send the whole struct on any change; the
                // engine echoes it back in the next RadioState.
                let mut cfg = self.state.skimmer;
                // A grid so the squelch fields line up under each other despite
                // the kind chips having different widths.
                egui::Grid::new("skimmer-kinds").num_columns(3).spacing([6.0, 5.0]).show(ui, |ui| {
                    if !wideband {
                        ui.disable();
                    }
                    for kind in SkimmerKind::ALL {
                        if crate::chrome::chip(ui, cfg.enabled(kind), kind.label())
                            .on_hover_text("Run this skimmer")
                            .clicked()
                        {
                            cfg.set_enabled(kind, !cfg.enabled(kind));
                        }
                        ui.label(RichText::new("sql").size(10.0).color(crate::theme::CYAN_DIM));
                        let mut sql = cfg.squelch_db(kind);
                        if ui
                            .add(DragValue::new(&mut sql).speed(0.25).range(0..=40).suffix(" dB"))
                            .on_hover_text("Minimum SNR a decoded signal needs to be spotted")
                            .changed()
                        {
                            cfg.set_squelch_db(kind, sql);
                        }
                        ui.end_row();
                    }
                });
                if !wideband {
                    ui.label(
                        RichText::new("needs a wideband IQ source")
                            .size(9.5)
                            .color(Color32::from_gray(150)),
                    );
                }
                if cfg != self.state.skimmer {
                    cmds.push(Command::SetSkimmerConfig(cfg));
                }
            });
        if let Some(r) = &resp {
            crate::chrome::paint_popup_cut_border(ui.ctx(), &r.response, alpha);
            if r.response.contains_pointer() {
                self.skimmer_popup_since = Some(now); // keep it up while the pointer is on it
            }
        }
    }

    /// The ☀ 3D chip: the solar-system view, in whichever window the platform
    /// gives us.
    ///
    /// Natively that is a second OS window this process owns, so the chip is a
    /// toggle and lights while it is open. In the browser it is a second tab,
    /// which the browser owns — we cannot know whether it is still open and
    /// clicking again should give you another one, so it is a plain button.
    /// The URL is relative so it survives any host, port or reverse proxy.
    fn solar_button(&mut self, ui: &mut egui::Ui) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            if crate::chrome::chip(ui, self.solar.open, "☀ 3D")
                .on_hover_text(
                    "Solar system 3D view — Sun, Earth, Moon, sunspots and CMEs (separate window)",
                )
                .clicked()
            {
                self.solar.open = !self.solar.open;
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            if crate::chrome::chip(ui, false, "☀ 3D")
                .on_hover_text(
                    "Solar system 3D view — Sun, Earth, Moon, sunspots and CMEs (new browser tab)",
                )
                .clicked()
            {
                ui.ctx().open_url(egui::OpenUrl::new_tab("?view=solar"));
            }
        }
    }

    fn display_module(&mut self, ui: &mut egui::Ui, cmds: &mut Vec<Command>) {
        // The module reserves its width before the content is drawn.
        const DISPLAY_W: f32 = 348.0;

        crate::chrome::module(ui, "Display", DISPLAY_W, |ui| {
            if crate::chrome::chip(ui, false, "FIT")
                .on_hover_text("Auto-set floor/ceiling for best waterfall contrast")
                .clicked()
            {
                self.auto_levels();
            }
            if crate::chrome::chip(ui, self.view.peak_hold, "PEAK")
                .on_hover_text("Decaying peak-hold trace")
                .clicked()
            {
                self.view.peak_hold = !self.view.peak_hold;
            }
            // Lit when the spectrum line is visible (not collapsed).
            if crate::chrome::chip(ui, !self.view.spectrum_collapsed, "SPEC")
                .on_hover_text("Show/hide the spectrum line above the waterfall")
                .clicked()
            {
                self.view.spectrum_collapsed = !self.view.spectrum_collapsed;
            }
            self.skimmer_button(ui, cmds);
            self.solar_button(ui);
            // Floor/ceiling + FFT size live in a popup off this button.
            let fft_btn = crate::chrome::chip(ui, false, "FFT")
                .on_hover_text("Spectrum floor / ceiling and FFT size");
            let fft_id = egui::Popup::default_response_id(&fft_btn);
            let now = ui.input(|i| i.time);
            let alpha =
                crate::chrome::popup_fade_alpha(ui.ctx(), fft_id, now, &mut self.fft_popup_since);
            let fft_resp = egui::Popup::from_toggle_button_response(&fft_btn)
                .frame(crate::chrome::window_frame_alpha(alpha))
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                .show(|ui| {
                    ui.set_opacity(alpha);
                    ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
                    ui.label(
                        RichText::new("SPECTRUM").color(crate::theme::CYAN_DIM).size(9.5).strong(),
                    );
                    ui.horizontal(|ui| {
                        ui.label("floor");
                        ui.add(
                            DragValue::new(&mut self.view.db_floor)
                                .speed(1.0)
                                .range(-160.0..=-40.0)
                                .suffix(" dB"),
                        );
                        ui.label("ceil");
                        ui.add(
                            DragValue::new(&mut self.view.db_ceil)
                                .speed(1.0)
                                .range(-100.0..=20.0)
                                .suffix(" dB"),
                        );
                    });
                    // Chips rather than a ComboBox: the combo opens a second popup
                    // layer, and clicking it counts as "outside" and closes this one.
                    ui.label(
                        RichText::new("FFT SIZE").color(crate::theme::CYAN_DIM).size(9.5).strong(),
                    );
                    ui.horizontal_wrapped(|ui| {
                        for n in [2048u32, 4096, 8192, 16384, 32768] {
                            if crate::chrome::chip(ui, self.view.fft_size == n, format!("{n}"))
                                .clicked()
                            {
                                self.view.fft_size = n;
                            }
                        }
                    });
                });
            if let Some(r) = &fft_resp {
                crate::chrome::paint_popup_cut_border(ui.ctx(), &r.response, alpha);
                if r.response.contains_pointer() {
                    self.fft_popup_since = Some(now);
                }
            }
        });
    }

    fn windows_module(&mut self, ui: &mut egui::Ui) {
        crate::chrome::module(ui, "System", 285.0, |ui| {
            if crate::chrome::chip(ui, self.show_logbook, "LOG")
                .on_hover_text("Logbook — all QSOs (digital + manual)")
                .clicked()
            {
                self.show_logbook = !self.show_logbook;
            }
            if crate::chrome::chip(ui, self.show_spots, "SPOTS")
                .on_hover_text("Live spots — DX cluster, POTA, SOTA, PSK Reporter")
                .clicked()
            {
                self.show_spots = !self.show_spots;
            }
            if crate::chrome::chip(ui, self.show_awards, "AWARDS")
                .on_hover_text("Award tracking — DXCC / WAS / WAZ / grids")
                .clicked()
            {
                self.show_awards = !self.show_awards;
            }
            if crate::chrome::chip(ui, self.show_memories, "MEM")
                .on_hover_text("Memory channels")
                .clicked()
            {
                self.show_memories = !self.show_memories;
            }
            if crate::chrome::chip(ui, self.show_settings, "⚙ SETTINGS")
                .on_hover_text("Settings — device gains, antennas, audio devices")
                .clicked()
            {
                self.show_settings = !self.show_settings;
            }
            if crate::chrome::chip(ui, self.help.open, "? HELP")
                .on_hover_text("User manual (F1)")
                .clicked()
            {
                self.help.open = !self.help.open;
            }
        });
    }

    /// Center the view on the tuned frequency after big jumps (band change,
    /// memory recall, startup) — i.e. whenever the tuning changed AND left
    /// the visible span. Deliberate pans away from the VFO are never
    /// snapped back, and drag-tuning keeps the VFO in view by itself.
    fn recenter_if_tuned_away(&mut self, prev_vfo: f64) {
        let vfo = self.state.active_freq_hz();
        let first = !self.seen_first_state;
        self.seen_first_state = true;
        if self.view.is_unset() {
            return; // spectrum_view will fit and center on first draw
        }
        let moved = (vfo - prev_vfo).abs() > 0.5;
        let outside = !(self.view.view_lo_hz..=self.view.view_hi_hz).contains(&vfo);
        if (moved || first) && outside {
            let span = self.view.span().min(self.state.sample_rate);
            self.view.view_lo_hz = vfo - span / 2.0;
            self.view.view_hi_hz = vfo + span / 2.0;
        }
    }

    /// Dispatch keyboard and mouse-button bindings for this frame.
    ///
    /// The bindings themselves live in `input.json` (see
    /// [`crate::input::InputRuntime`]); the shipped defaults reproduce the
    /// shortcuts that used to be hardcoded here — ←/→ ±100 Hz (Shift: ±10),
    /// ↑/↓ ±1 kHz, PgUp/PgDn ±10 kHz, M mute, N noise blanker, F fit span.
    fn control_inputs(&mut self, ctx: &egui::Context, cmds: &mut Vec<Command>) {
        // Destructured rather than borrowed field-by-field: the runtime needs
        // `state` and the window flags mutably at the same time, and they are
        // disjoint parts of `self`.
        let SdroxideApp {
            input,
            state,
            view,
            help,
            show_settings,
            show_logbook,
            show_spots,
            show_memories,
            show_voice,
            ..
        } = self;
        let mut sink = crate::input::UiSink {
            view,
            help: &mut help.open,
            settings: show_settings,
            logbook: show_logbook,
            spots: show_spots,
            memories: show_memories,
            voice: show_voice,
        };
        input.poll_pointer_and_keys(ctx, state, &mut sink, cmds);
        #[cfg(not(target_arch = "wasm32"))]
        input.poll_midi(ctx, state, &mut sink, cmds);
    }

    /// De-assert every held control. Closing the window while a footswitch or
    /// a bound key is down must not leave the transmitter keyed.
    fn release_held_controls(&mut self, cmds: &mut Vec<Command>) {
        let SdroxideApp {
            input,
            state,
            view,
            help,
            show_settings,
            show_logbook,
            show_spots,
            show_memories,
            show_voice,
            ..
        } = self;
        let mut sink = crate::input::UiSink {
            view,
            help: &mut help.open,
            settings: show_settings,
            logbook: show_logbook,
            spots: show_spots,
            memories: show_memories,
            voice: show_voice,
        };
        input.release_all(state, &mut sink, cmds);
    }

    /// The FT8/FT4 operating panel: decode list on the left, QSO area on the
    /// right. Sits below the (zoomed) waterfall in digital modes.
    fn digi_panel(&mut self, ui: &mut egui::Ui, cmds: &mut Vec<Command>) {
        let avail = ui.available_size();
        let handle_w = 7.0;
        // Decode list takes a user-draggable fraction of the width; the QSO area
        // gets the rest (each keeps a usable minimum).
        let left_w = (avail.x * self.view.digi_split_fraction)
            .clamp(180.0, (avail.x - handle_w - 220.0).max(180.0));
        ui.horizontal_top(|ui| {
            // Force a top-down layout: `allocate_ui` would otherwise inherit the
            // parent `horizontal_top` (left-to-right) and lay the rows out
            // sideways, overflowing and shoving the QSO column off-screen.
            ui.allocate_ui_with_layout(
                egui::vec2(left_w, avail.y),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    self.decode_list(ui, cmds);
                },
            );
            // Draggable vertical divider between the decode table and the QSO area.
            let (hrect, hresp) =
                ui.allocate_exact_size(egui::vec2(handle_w, avail.y), egui::Sense::click_and_drag());
            if hresp.hovered() || hresp.dragged() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
            }
            if hresp.dragged() {
                let d = hresp.drag_delta().x / avail.x.max(1.0);
                self.view.digi_split_fraction = (self.view.digi_split_fraction + d).clamp(0.28, 0.72);
            }
            {
                let p = ui.painter_at(hrect);
                let hot = hresp.hovered() || hresp.dragged();
                let col = if hot { crate::theme::CYAN } else { Color32::from_gray(70) };
                let (cx, cy) = (hrect.center().x, hrect.center().y);
                for dy in [-16.0f32, 0.0, 16.0] {
                    p.line_segment(
                        [egui::pos2(cx, cy + dy - 6.0), egui::pos2(cx, cy + dy + 6.0)],
                        egui::Stroke::new(2.0, col),
                    );
                }
            }
            ui.vertical(|ui| {
                self.qso_area(ui, cmds);
            });
        });
    }

    /// Touch-friendly decode list with a per-row REPLY button. Clicking a
    /// row moves the TX audio frequency to that signal; REPLY starts a QSO.
    fn decode_list(&mut self, ui: &mut egui::Ui, cmds: &mut Vec<Command>) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("DECODES").size(9.5).strong().color(crate::theme::CYAN_DIM));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new(format!("{} rx", self.digi_decodes.len()))
                        .size(10.0)
                        .color(Color32::from_gray(120)),
                );
            });
        });
        // Per-turn ordering + a CQ-only filter for the decode list.
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            ui.label(RichText::new("Sort").size(9.5).color(crate::theme::CYAN_DIM));
            for (m, base) in
                [(DecodeSort::None, "None"), (DecodeSort::Signal, "SNR"), (DecodeSort::Distance, "Dist")]
            {
                let active = self.digi_sort == m;
                // Active mode shows its direction; re-pressing it flips direction.
                let lbl = if active && m != DecodeSort::None {
                    format!("{base} {}", if self.digi_sort_desc { "↓" } else { "↑" })
                } else {
                    base.to_string()
                };
                if crate::chrome::chip(ui, active, lbl).clicked() {
                    if active && m != DecodeSort::None {
                        self.digi_sort_desc = !self.digi_sort_desc;
                    } else {
                        self.digi_sort = m;
                        self.digi_sort_desc = true; // default: strongest / farthest first
                    }
                }
            }
            ui.add_space(8.0);
            if crate::chrome::chip(ui, self.digi_cq_only, "CQ only").clicked() {
                self.digi_cq_only = !self.digi_cq_only;
            }
            if crate::chrome::chip(ui, self.digi_new_only, "New only")
                .on_hover_text("Only stations that would be new: entity, band-slot, grid, or call")
                .clicked()
            {
                self.digi_new_only = !self.digi_new_only;
            }
        });
        ui.add_space(2.0);
        // Call of the currently previewed decode (cloned so the scroll closure
        // doesn't hold a borrow of `self` we need to write back afterwards).
        let preview_call = self.digi_preview.as_ref().map(|(c, _)| c.clone());
        // Own grid, for the per-decode great-circle distance column.
        let my_grid = self.digi_status.as_ref().map(|s| s.config.my_grid.clone()).unwrap_or_default();
        // Own callsign, to spotlight decodes addressed to us.
        let my_call = self.digi_status.as_ref().map(|s| s.config.my_call.clone()).unwrap_or_default();
        // Staged preview change: `None` = no click this frame; `Some(v)` =
        // replace the preview with `v` (`Some(None)` clears it).
        let mut new_preview: Option<Option<(String, (f64, f64))>> = None;
        // Location of the row hovered this frame → yellow dot on the map.
        let mut hover_ll: Option<(f64, f64)> = None;
        let cq_only = self.digi_cq_only;
        let new_only = self.digi_new_only;
        let sort = self.digi_sort;
        let desc = self.digi_sort_desc;
        // Turn parity needs the mode's slot length (FT8 15 s, FT4 7.5 s).
        let period = if self.state.rx[0].mode == Mode::Ft4 { 7.5 } else { 15.0 };
        // The band we'd log a contact on, for the "is this one new?" judgement.
        let dial_hz = self.state.active_freq_hz();
        let band = if dial_hz > 0.0 { sdroxide_types::adif_band(dial_hz) } else { "" };
        // Refresh the log index (needs &mut self), then borrow it beside the
        // decodes for the per-row novelty lookups.
        self.log_index();
        let log_ix = &self.log_index_cache.as_ref().expect("just refreshed").1;
        // Filter (CQ-only / new-only) and precompute distance for sorting and
        // display. Entries stay newest-turn-first; same-slot decodes are
        // contiguous in the list. A "CQ DX" from a station we're local to is not
        // a CQ *we* can answer: it neither passes the filter nor gets the CQ
        // highlight.
        let mut items: Vec<(usize, &Decode, Option<f64>, bool, sdroxide_types::Novelty)> = self
            .digi_decodes
            .iter()
            .enumerate()
            .filter_map(|(i, d)| {
                let cq = sdroxide_types::cq_is_for_us(d, &my_call, &my_grid);
                if cq_only && !cq {
                    return None;
                }
                let novelty =
                    log_ix.novelty(d.from.as_deref().unwrap_or(""), d.grid.as_deref(), band);
                if new_only && !novelty.is_new() {
                    return None;
                }
                let dist = (!my_grid.is_empty())
                    .then(|| {
                        d.grid.as_deref().and_then(|g| sdroxide_types::grid_distance_km(&my_grid, g))
                    })
                    .flatten();
                Some((i, d, dist, cq, novelty))
            })
            .collect();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show_themed(ui, |ui| {
            let mut gi = 0;
            while gi < items.len() {
                // A turn is one slot: group the contiguous same-slot decodes.
                let slot = items[gi].1.slot_utc;
                let mut end = gi;
                while end < items.len() && items[end].1.slot_utc == slot {
                    end += 1;
                }
                match sort {
                    DecodeSort::None => {}
                    DecodeSort::Signal => items[gi..end].sort_by(|a, b| {
                        let o = a.1.snr_db.cmp(&b.1.snr_db);
                        if desc { o.reverse() } else { o }
                    }),
                    DecodeSort::Distance => items[gi..end].sort_by(|a, b| {
                        // Decodes without a grid always sort last (push them to the
                        // far end of whichever direction is active).
                        let sentinel = if desc { f64::NEG_INFINITY } else { f64::INFINITY };
                        let ka = a.2.unwrap_or(sentinel);
                        let kb = b.2.unwrap_or(sentinel);
                        let o = ka.partial_cmp(&kb).unwrap_or(std::cmp::Ordering::Equal);
                        if desc { o.reverse() } else { o }
                    }),
                }
                // Turn separator: even/odd parity + UTC timestamp.
                let even = ((slot as f64 / period).round() as i64).rem_euclid(2) == 0;
                let s = slot.rem_euclid(86_400);
                let tstr = format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60);
                ui.add_space(3.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 5.0;
                    let (ptxt, pcol) = if even {
                        ("EVEN", crate::theme::CYAN)
                    } else {
                        ("ODD", crate::theme::YELLOW)
                    };
                    ui.label(RichText::new(ptxt).size(9.0).strong().color(pcol));
                    ui.label(
                        RichText::new(format!("{tstr} UTC"))
                            .size(9.5)
                            .monospace()
                            .color(Color32::from_gray(130)),
                    );
                });
                ui.separator();
                for k in gi..end {
                    let (i, d, dist_km, cq, novelty) = items[k];
                    // Decodes addressed to our own station stand out most.
                    let to_me = !my_call.is_empty() && d.to.as_deref() == Some(my_call.as_str());
                    // Free text names no sender, and a hashed callsign nobody
                    // has heard yet resolves to none either — say which it is
                    // rather than showing a bare "?".
                    let who = d.from.clone().unwrap_or_else(|| {
                        if d.free_text { "TEXT".into() } else { "?".into() }
                    });
                    // What this station would be worth working: one badge, and
                    // a dupe fades the row back so the new ones carry the eye.
                    let (badge, badge_col) = match novelty.highlight() {
                        Some(sdroxide_types::Highlight::NewDxcc) => ("DXCC", crate::theme::PINK),
                        Some(sdroxide_types::Highlight::NewDxccBand) => {
                            ("BAND", crate::theme::YELLOW)
                        }
                        Some(sdroxide_types::Highlight::NewGrid) => ("GRID", crate::theme::CYAN),
                        Some(sdroxide_types::Highlight::NewCall) => ("NEW", crate::theme::CYAN_DIM),
                        Some(sdroxide_types::Highlight::Dupe) => ("DUPE", Color32::from_gray(85)),
                        None => ("", Color32::TRANSPARENT),
                    };
                    let dupe = novelty.dupe;
                    let grid = d.grid.clone().unwrap_or_default();
                    let is_preview =
                        d.from.is_some() && preview_call.as_deref() == d.from.as_deref();
                let mut reply = false;
                // Left edge of the REPLY button, so the row-body click area can
                // exclude it (otherwise the full-row interaction below sits on
                // top of the button and swallows its clicks).
                let mut reply_left: Option<f32> = None;

                let inner = egui::Frame::new()
                    .fill(if to_me {
                        crate::theme::TOME_BG
                    } else if cq {
                        crate::theme::CQ_BG
                    } else {
                        crate::theme::ROW_BG
                    })
                    .inner_margin(egui::Margin { left: 11, right: 6, top: 6, bottom: 6 })
                    .show(ui, |ui| {
                        // Fixed-width columns so every field lines up down the
                        // list. Right-aligned numbers, then callsign (wide
                        // proportional font), grid, and the message filling the
                        // rest with a right-pinned REPLY button.
                        let ch = 22.0;
                        ui.horizontal(|ui| {
                            ui.set_min_height(ch);
                            ui.spacing_mut().item_spacing.x = 7.0;
                            let cell = |ui: &mut egui::Ui, w: f32, align_right: bool, lbl: egui::Label| {
                                // Reserve the column width *exactly*: a plain
                                // allocate_ui shrinks to its content, so a short
                                // callsign would collapse the column and shift
                                // the grid + message out of alignment.
                                let (rect, _) =
                                    ui.allocate_exact_size(egui::vec2(w, ch), egui::Sense::hover());
                                let layout = if align_right {
                                    egui::Layout::right_to_left(egui::Align::Center)
                                } else {
                                    egui::Layout::left_to_right(egui::Align::Center)
                                };
                                let mut child =
                                    ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(layout));
                                child.add(lbl);
                            };
                            // SNR.
                            cell(
                                ui,
                                28.0,
                                true,
                                egui::Label::new(
                                    RichText::new(format!("{:+}", d.snr_db))
                                        .monospace()
                                        .size(13.0)
                                        .color(snr_color(d.snr_db)),
                                ),
                            );
                            // Audio frequency.
                            cell(
                                ui,
                                40.0,
                                true,
                                egui::Label::new(
                                    RichText::new(format!("{:.0}", d.audio_hz))
                                        .monospace()
                                        .size(12.0)
                                        .color(Color32::from_gray(120)),
                                ),
                            );
                            // Callsign — wider proportional (button) font.
                            cell(
                                ui,
                                98.0,
                                false,
                                egui::Label::new(
                                    RichText::new(&who).size(15.0).strong().color(if to_me {
                                        crate::theme::YELLOW
                                    } else if d.from.is_none() || dupe {
                                        Color32::from_gray(105)
                                    } else if cq {
                                        crate::theme::GREEN
                                    } else {
                                        crate::theme::TEXT_STRONG
                                    }),
                                )
                                .truncate(),
                            );
                            // What they'd be worth: new entity / band / grid /
                            // call, or a dupe already in the log for this band.
                            cell(
                                ui,
                                34.0,
                                false,
                                egui::Label::new(
                                    RichText::new(badge).size(9.5).strong().color(badge_col),
                                ),
                            );
                            // Grid.
                            cell(
                                ui,
                                44.0,
                                false,
                                egui::Label::new(
                                    RichText::new(&grid)
                                        .monospace()
                                        .size(12.0)
                                        .color(crate::theme::CYAN_DIM),
                                ),
                            );
                            // Distance (km, great-circle from my grid).
                            cell(
                                ui,
                                58.0,
                                true,
                                egui::Label::new(
                                    RichText::new(
                                        dist_km
                                            .map(|km| format!("{km:.0} km"))
                                            .unwrap_or_default(),
                                    )
                                    .monospace()
                                    .size(11.0)
                                    .color(crate::theme::YELLOW),
                                ),
                            );
                            // Message fills the remaining width; REPLY pinned right.
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let resp = crate::chrome::chip_accent(
                                    ui,
                                    false,
                                    RichText::new("REPLY").size(12.0).strong(),
                                    if to_me {
                                        crate::theme::YELLOW
                                    } else if cq {
                                        crate::theme::GREEN
                                    } else {
                                        crate::theme::CYAN
                                    },
                                    crate::theme::INK_ON_CYAN,
                                );
                                reply = resp.clicked();
                                reply_left = Some(resp.rect.left());
                                ui.with_layout(
                                    egui::Layout::left_to_right(egui::Align::Center),
                                    |ui| {
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(&d.message)
                                                    .monospace()
                                                    .size(12.5)
                                                    .color(if dupe {
                                                        Color32::from_gray(95)
                                                    } else {
                                                        crate::theme::TEXT
                                                    }),
                                            )
                                            .truncate(),
                                        );
                                    },
                                );
                            });
                        });
                    });

                let r = inner.response.rect;
                // Left-accent bar: gold (to us) / red (CQ) / cyan (other). Wider
                // for a to-us decode so it really pops.
                let (accent, aw) = if to_me {
                    (crate::theme::YELLOW, 4.0)
                } else if cq {
                    (crate::theme::PINK, 2.5)
                } else {
                    (crate::theme::CYAN_DIM, 2.5)
                };
                ui.painter().rect_filled(
                    egui::Rect::from_min_max(r.left_top(), egui::pos2(r.left() + aw, r.bottom())),
                    0.0,
                    accent,
                );
                // Row-body click (everything left of the REPLY button) tunes
                // the audio freq. Excluding the button's rect keeps this
                // interaction from covering — and stealing clicks from — REPLY.
                let body_right = reply_left.map(|x| x - 2.0).unwrap_or(r.right());
                let body_rect = egui::Rect::from_min_max(r.left_top(), egui::pos2(body_right, r.bottom()));
                let row = ui.interact(body_rect, ui.id().with(("dec", i)), egui::Sense::click());
                if is_preview {
                    // Amber outline ties this row to its faint map marker.
                    ui.painter().rect_stroke(
                        r,
                        0.0,
                        egui::Stroke::new(1.0, crate::theme::YELLOW),
                        egui::StrokeKind::Inside,
                    );
                } else if to_me {
                    // A message to our own station: a gold box so it can't be missed.
                    ui.painter().rect_stroke(
                        r,
                        0.0,
                        egui::Stroke::new(1.4, crate::theme::YELLOW),
                        egui::StrokeKind::Inside,
                    );
                } else if row.hovered() {
                    ui.painter().rect_stroke(
                        r,
                        0.0,
                        egui::Stroke::new(1.0, crate::theme::CYAN_DIM),
                        egui::StrokeKind::Inside,
                    );
                }
                if row.hovered() {
                    hover_ll = d.grid.as_deref().and_then(sdroxide_types::grid_to_latlon);
                }
                if reply {
                    if let Some(from) = &d.from {
                        // If they're neither calling CQ nor calling us, hold until
                        // they call CQ rather than barging into their exchange.
                        // Any CQ counts here, including a "CQ DX" we're local to:
                        // the operator asked to call them, and they are free now.
                        cmds.push(Command::DigiStartQso {
                            from: from.clone(),
                            grid: d.grid.clone(),
                            snr: d.snr_db,
                            audio_hz: d.audio_hz,
                            wait_for_cq: !d.is_cq && !to_me,
                        });
                    }
                    // Starting a QSO promotes the station to the active DX
                    // marker; drop the faint preview so they don't overlap.
                    new_preview = Some(None);
                } else if row.clicked() {
                    cmds.push(Command::SetDigiAudioFreq(d.audio_hz));
                    // Preview this station's location (if it sent a grid).
                    let ll = d.grid.as_deref().and_then(sdroxide_types::grid_to_latlon);
                    new_preview = Some(ll.map(|ll| (who.clone(), ll)));
                }
                    ui.add_space(3.0);
                }
                gi = end;
            }
        });
        self.digi_hover_ll = hover_ll;
        if let Some(sel) = new_preview {
            self.digi_preview = sel;
        }
    }

    /// The QSO operating area to the right of the decode list: header row,
    /// world map, station card, transcript, and action buttons.
    fn qso_area(&mut self, ui: &mut egui::Ui, cmds: &mut Vec<Command>) {
        let status = self.digi_status.clone();
        let in_qso = status
            .as_ref()
            .map(|s| {
                !matches!(
                    s.step,
                    sdroxide_types::QsoStep::Idle
                        | sdroxide_types::QsoStep::Confirming
                        | sdroxide_types::QsoStep::Done
                )
            })
            .unwrap_or(false);

        // Header: QSO left, session log + downloads centered, SETUP right.
        let logged = self.qso_log.len();
        let row_h = 26.0;
        let (row, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), egui::Sense::hover());
        let third = row.width() / 3.0;
        let zone = |i: f32| {
            egui::Rect::from_min_size(
                egui::pos2(row.left() + i * third, row.top()),
                egui::vec2(third, row_h),
            )
        };
        ui.scope_builder(
            egui::UiBuilder::new().max_rect(zone(0.0)).layout(egui::Layout::left_to_right(egui::Align::Center)),
            |ui| {
                ui.label(RichText::new("QSO").size(9.5).strong().color(crate::theme::CYAN_DIM));
            },
        );
        ui.scope_builder(
            egui::UiBuilder::new().max_rect(zone(1.0)).layout(egui::Layout::top_down(egui::Align::Center)),
            |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("Session: {logged} QSO"))
                            .size(11.0)
                            .color(Color32::from_gray(150)),
                    );
                    if ui.add_enabled(logged > 0, egui::Button::new("ADIF")).clicked() {
                        let adif = sdroxide_types::qso_log_to_adif(&self.qso_log);
                        crate::download::save("sdroxide-log.adi", adif.as_bytes());
                    }
                    if ui.add_enabled(logged > 0, egui::Button::new("TXT")).clicked() {
                        let txt = sdroxide_types::qso_log_to_text(&self.qso_log);
                        crate::download::save("sdroxide-log.txt", txt.as_bytes());
                    }
                });
            },
        );
        ui.scope_builder(
            egui::UiBuilder::new().max_rect(zone(2.0)).layout(egui::Layout::right_to_left(egui::Align::Center)),
            |ui| {
                if crate::chrome::chip(ui, self.show_digi_settings, "⚙ SETUP").clicked() {
                    self.show_digi_settings = !self.show_digi_settings;
                }
            },
        );

        ui.add_space(5.0);
        // World map — its height is a user-draggable fraction of the QSO area,
        // clamped so the station card + a usable transcript + the action buttons
        // stay visible. On very short windows it shrinks and then disappears.
        let btn_h = 44.0;
        let gap = 8.0;
        let map_handle_h = 7.0;
        const CARD_RESERVE: f32 = 60.0;
        let full_h = ui.available_height();
        let avail_w = ui.available_width();
        // The map height is a user-draggable fraction of a range: from MIN_HEIGHT
        // up to whatever still leaves the station card + action buttons room below
        // (the transcript can shrink to nothing). `digi_map_fraction` slides
        // linearly across this range, so the divider tracks the cursor 1:1 and the
        // map genuinely grows/shrinks. Height is capped at the width (aspect ≤ 1).
        let map_lo = crate::widgets::worldmap::MIN_HEIGHT;
        let map_hi = (full_h - (map_handle_h + CARD_RESERVE + 5.0 + gap + btn_h))
            .min(avail_w)
            .max(map_lo);
        let map_budget = map_lo + (map_hi - map_lo) * self.view.digi_map_fraction;
        let hover_ll = self.digi_hover_ll;
        let my_grid = status.as_ref().map(|s| s.config.my_grid.clone()).unwrap_or_default();
        let home_ll = sdroxide_types::grid_to_latlon(&my_grid);
        let dx_grid = status.as_ref().and_then(|s| s.dx_grid.clone());
        let dx_ll = dx_grid.as_deref().and_then(sdroxide_types::grid_to_latlon);
        // A clicked (but not yet answered) decode shows as a faint preview.
        let preview_ll = self.digi_preview.as_ref().map(|(_, ll)| *ll);
        let tx_active = status.as_ref().map(|s| s.transmitting).unwrap_or(false);
        // White station dots fade over 2 minutes since a station was last
        // decoded, then expire (dropped from the map and from the zoom fit).
        // Ages use egui frame time (monotonic, works native + wasm); each grid
        // remembers the frame it was last freshly decoded in.
        let now_t = ui.input(|i| i.time);
        self.digi_stations.observe(&self.digi_decodes, now_t);
        let stations = self.digi_stations.stations(now_t);
        // Located network spots (filtered by the shown-kind toggles), as
        // kind-coloured dots on the map.
        let spot_dots: Vec<(f64, f64, (u8, u8, u8))> = self
            .spots
            .iter()
            .filter(|s| self.spot_kinds_shown[spot_kind_index(s.kind)])
            .filter_map(|s| s.loc.map(|(lat, lon)| (lat, lon, s.kind.color())))
            .collect();
        if map_budget >= crate::widgets::worldmap::MIN_HEIGHT {
            crate::widgets::worldmap::show(
                ui,
                &mut self.map_view,
                home_ll,
                dx_ll,
                preview_ll,
                hover_ll,
                &stations,
                &spot_dots,
                tx_active,
                map_budget,
            );
            // Draggable border between the map and the QSO form below it.
            let (hrect, hresp) = ui
                .allocate_exact_size(egui::vec2(ui.available_width(), map_handle_h), egui::Sense::click_and_drag());
            if hresp.hovered() || hresp.dragged() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
            }
            if hresp.dragged() {
                // 1:1 with the cursor: a drag of `dy` px moves the map edge `dy` px.
                let df = hresp.drag_delta().y / (map_hi - map_lo).max(1.0);
                self.view.digi_map_fraction = (self.view.digi_map_fraction + df).clamp(0.0, 1.0);
            }
            {
                let p = ui.painter_at(hrect);
                let hot = hresp.hovered() || hresp.dragged();
                let col = if hot { crate::theme::CYAN } else { Color32::from_gray(70) };
                let (cx, cy) = (hrect.center().x, hrect.center().y);
                for dx in [-16.0f32, 0.0, 16.0] {
                    p.line_segment(
                        [egui::pos2(cx + dx - 6.0, cy), egui::pos2(cx + dx + 6.0, cy)],
                        egui::Stroke::new(2.0, col),
                    );
                }
            }
        }
        // Station card.
        crate::chrome::red_panel(ui, |ui| {
            match status.as_ref() {
                Some(s) => {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(s.step.label()).size(13.0).strong().color(crate::theme::CYAN));
                        if s.transmitting {
                            ui.label(RichText::new("● TX").size(13.0).strong().color(crate::theme::PINK));
                        }
                        if s.tx_watchdog {
                            // The sequencer stood down on its own; say so, since
                            // an idle step alone looks like nothing happened.
                            ui.label(
                                RichText::new("WATCHDOG")
                                    .size(11.0)
                                    .strong()
                                    .color(crate::theme::YELLOW),
                            )
                            .on_hover_text(
                                "Transmitting stopped: no reply and no action for the watchdog \
                                 period. Call CQ or pick a message to resume.",
                            );
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                RichText::new(format!(
                                    "{:.0} Hz · {} slots",
                                    s.audio_hz,
                                    if s.tx_even { "even" } else { "odd" }
                                ))
                                .size(11.0)
                                .color(Color32::from_gray(140)),
                            );
                        });
                    });
                    match &s.dx_call {
                        Some(dx) => {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(dx)
                                        .size(17.0)
                                        .strong()
                                        .color(crate::theme::TEXT_STRONG),
                                );
                                if let Some(g) = &s.dx_grid {
                                    ui.label(RichText::new(g).size(13.0).color(crate::theme::CYAN_DIM));
                                }
                                if let (Some(hg), Some(dg)) = (
                                    (!my_grid.is_empty()).then_some(my_grid.as_str()),
                                    s.dx_grid.as_deref(),
                                ) {
                                    if let (Some(km), Some(brg)) = (
                                        sdroxide_types::grid_distance_km(hg, dg),
                                        sdroxide_types::grid_bearing(hg, dg),
                                    ) {
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                ui.label(
                                                    RichText::new(format!(
                                                        "{:.0} km · {:.0}°",
                                                        km, brg
                                                    ))
                                                    .size(12.0)
                                                    .color(crate::theme::YELLOW),
                                                );
                                            },
                                        );
                                    }
                                }
                            });
                        }
                        None => {
                            ui.label(
                                RichText::new("no active QSO — pick a decode to reply, or Call CQ")
                                    .size(11.0)
                                    .color(Color32::from_gray(120)),
                            );
                        }
                    }
                }
                None => {
                    ui.label(RichText::new("FT8 engine idle").size(12.0).color(Color32::from_gray(130)));
                }
            }
        });

        // Transcript: a red-bordered scroll box that always fills the space
        // between the station card and the action buttons (reserve the button
        // row height first, give the rest to the transcript).
        ui.add_space(5.0);
        // Reserve the button row (+gap) at the bottom so the action buttons stay
        // visible no matter how short the window is; the transcript takes the
        // rest. Floor at 0 (not a fixed minimum) so a very short window shrinks
        // the conversation rather than pushing the buttons off-screen.
        let msg_row_h = 26.0;
        let trans_h = (ui.available_height() - btn_h - msg_row_h - 2.0 * gap).max(0.0);
        ui.allocate_ui(egui::vec2(ui.available_width(), trans_h), |ui| {
            let inner = egui::Frame::new()
                .fill(crate::theme::ROW_BG)
                .stroke(egui::Stroke::new(1.0, crate::theme::RED_DEEP))
                .inner_margin(egui::Margin { left: 9, right: 7, top: 6, bottom: 6 })
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.set_min_height(ui.available_height());
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .stick_to_bottom(true)
                        .show_themed(ui, |ui| {
                            let mut any = false;
                            if let Some(s) = status.as_ref() {
                                for line in &s.transcript {
                                    any = true;
                                    // Pink marks traffic that isn't ours: the
                                    // station we called is working someone else.
                                    let (tag, col) = if line.overheard {
                                        ("·", crate::theme::PINK)
                                    } else if line.tx {
                                        ("»", crate::theme::YELLOW)
                                    } else {
                                        ("«", crate::theme::GREEN)
                                    };
                                    ui.label(
                                        RichText::new(format!("{tag} {}", line.text))
                                            .monospace()
                                            .size(12.5)
                                            .color(col),
                                    );
                                }
                                if let Some(msg) = &s.tx_pending_msg {
                                    any = true;
                                    ui.label(
                                        RichText::new(format!("→ {msg}"))
                                            .monospace()
                                            .size(11.5)
                                            .color(Color32::from_gray(150)),
                                    );
                                }
                            }
                            if !any {
                                ui.label(
                                    RichText::new("— no messages —")
                                        .monospace()
                                        .size(11.5)
                                        .color(Color32::from_gray(90)),
                                );
                            }
                        });
                });
            // Red left-accent bar (matching chrome::red_panel).
            let r = inner.response.rect;
            ui.painter().rect_filled(
                egui::Rect::from_min_max(r.left_top(), egui::pos2(r.left() + 2.5, r.bottom())),
                0.0,
                crate::theme::PINK,
            );
        });

        ui.add_space(gap);
        // Message picker: choose by hand which message goes next (WSJT-X's
        // Tx1–Tx6), or send a line of free text in the next slot.
        let has_dx = status.as_ref().and_then(|s| s.dx_call.as_ref()).is_some();
        let step_now = status.as_ref().map(|s| s.step);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            for (step, label) in [
                (sdroxide_types::QsoStep::TxGrid, "GRID"),
                (sdroxide_types::QsoStep::TxReport, "RPT"),
                (sdroxide_types::QsoStep::TxRReport, "R+RPT"),
                (sdroxide_types::QsoStep::TxRr73, "RR73"),
                (sdroxide_types::QsoStep::Tx73, "73"),
            ] {
                let resp = ui.add_enabled_ui(has_dx, |ui| {
                    crate::chrome::chip(
                        ui,
                        step_now == Some(step),
                        RichText::new(label).size(11.0),
                    )
                });
                if resp.inner.clicked() {
                    cmds.push(Command::DigiSetStep(step));
                }
            }
            ui.add_space(4.0);
            // Free text: 13 characters is all FT8 carries, so cap the entry
            // there rather than letting the operator type a message that would
            // be silently cut on the air.
            let entry = ui.add(
                egui::TextEdit::singleline(&mut self.digi_free_text)
                    .desired_width(ui.available_width() - 52.0)
                    .char_limit(13)
                    .hint_text("free text (13 chars)"),
            );
            let send = crate::chrome::chip_accent(
                ui,
                false,
                RichText::new("SEND").size(11.0).strong(),
                crate::theme::CYAN,
                crate::theme::INK_ON_CYAN,
            );
            let entered = entry.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (send.clicked() || entered) && !self.digi_free_text.trim().is_empty() {
                cmds.push(Command::DigiSendText(self.digi_free_text.clone()));
                self.digi_free_text.clear();
            }
        });

        ui.add_space(gap);
        // Action buttons (larger for touch).
        ui.horizontal(|ui| {
            let cq = ui.add_enabled_ui(!in_qso, |ui| {
                crate::chrome::chip_accent(
                    ui,
                    false,
                    RichText::new("  CALL CQ  ").size(15.0).strong(),
                    crate::theme::GREEN,
                    crate::theme::INK_ON_CYAN,
                )
            });
            if cq.inner.clicked() {
                cmds.push(Command::DigiCallCq);
            }
            if crate::chrome::chip(ui, false, RichText::new(" STOP QSO ").size(14.0)).clicked() {
                cmds.push(Command::DigiStopQso);
            }
            if crate::chrome::chip_accent(
                ui,
                false,
                RichText::new(" STOP TX ").size(15.0).strong(),
                crate::theme::PINK,
                Color32::WHITE,
            )
            .clicked()
            {
                cmds.push(Command::DigiAbortTx);
            }
        });
    }

    /// A compact decode-squelch (sensitivity) slider for the keyboard panels:
    /// higher = require a stronger signal, so pure noise stops decoding.
    fn digi_squelch_slider(&mut self, ui: &mut egui::Ui, cmds: &mut Vec<Command>) {
        let mut sq = self.digi_cfg_edit.digi_squelch;
        ui.spacing_mut().slider_width = 84.0;
        let resp = ui
            .add(egui::Slider::new(&mut sq, 0.0..=1.0).show_value(false))
            .on_hover_text("Decode squelch — raise to stop decoding noise");
        ui.label(RichText::new("SQL").size(10.0).color(crate::theme::CYAN_DIM));
        if resp.changed() && self.digi_cfg_seeded {
            self.digi_cfg_edit.digi_squelch = sq;
            cmds.push(Command::SetDigiConfig(self.digi_cfg_edit.clone()));
        }
    }

    /// PSK/RTTY keyboard-mode panel: the decoded RX stream on top, then a
    /// streaming TX input (already-sent characters shown green) and controls.
    /// `panel_h` is the real bounded height (the surrounding frame reports an
    /// unbounded `available_height`, so we can't use it for the split).
    fn text_modem_panel(&mut self, ui: &mut egui::Ui, cmds: &mut Vec<Command>, panel_h: f32) {
        // Panel content bottom, from the entry cursor (frame margins accounted);
        // the larger margin leaves clear padding below the button row.
        let content_bottom = ui.cursor().top() + panel_h - 40.0;
        let status = self.digi_status.clone();
        let mode = self.state.rx[0].mode;
        let audio_hz = status.as_ref().map(|s| s.audio_hz).unwrap_or(1500.0);
        let sent = status.as_ref().map(|s| s.tx_sent).unwrap_or(0);
        let tx_on = status.as_ref().map(|s| s.tx_next).unwrap_or(false);
        let transmitting = status.as_ref().map(|s| s.transmitting).unwrap_or(false);
        let rx_text = status.as_ref().map(|s| s.text_rx.clone()).unwrap_or_default();
        let my_call = status.as_ref().map(|s| s.config.my_call.clone()).unwrap_or_default();

        // Header: mode + tuning readout / nudges, SETUP + TX indicator.
        ui.horizontal(|ui| {
            ui.label(RichText::new(mode.label()).size(11.0).strong().color(crate::theme::CYAN));
            ui.label(
                RichText::new(format!("{audio_hz:.0} Hz")).size(11.0).color(Color32::from_gray(150)),
            );
            if crate::chrome::chip(ui, false, "−").on_hover_text("Tune down 10 Hz").clicked() {
                cmds.push(Command::SetDigiAudioFreq((audio_hz - 10.0).clamp(200.0, 3500.0)));
            }
            if crate::chrome::chip(ui, false, "+").on_hover_text("Tune up 10 Hz").clicked() {
                cmds.push(Command::SetDigiAudioFreq((audio_hz + 10.0).clamp(200.0, 3500.0)));
            }
            // Mode parameters inline next to the tune buttons (RTTY shift/baud,
            // Olivia tones/bandwidth, THOR submode) — no separate setup dialog.
            self.text_modem_params_row(ui, cmds);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if transmitting {
                    ui.label(RichText::new("● TX").size(11.0).strong().color(crate::theme::PINK));
                }
                self.digi_squelch_slider(ui, cmds);
            });
        });
        ui.add_space(4.0);

        // Reserve the input + button rows at the bottom; RX stream gets the rest.
        // Sized against the real panel bottom (not the unbounded available_height)
        // so the fixed controls are never pushed off a short panel.
        let btn_h = 32.0;
        let input_h = 56.0; // fixed-height, internally-scrolling TX box
        let gap = 5.0;
        let bottom_pad = 12.0; // clear space below the button row
        let rx_h = (content_bottom - ui.cursor().top() - btn_h - input_h - 2.0 * gap - bottom_pad)
            .max(24.0);

        ui.allocate_ui(egui::vec2(ui.available_width(), rx_h), |ui| {
            egui::Frame::new()
                .fill(crate::theme::ROW_BG)
                .stroke(egui::Stroke::new(1.0, crate::theme::RED_DEEP))
                .inner_margin(egui::Margin { left: 8, right: 7, top: 6, bottom: 6 })
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.set_min_height(ui.available_height());
                    // Cap the scroll height explicitly (bounded `available_height`
                    // isn't reliable inside the auto-sizing frame) so long RX text
                    // scrolls instead of growing the panel.
                    egui::ScrollArea::vertical()
                        .max_height((rx_h - 12.0).max(20.0))
                        .auto_shrink([false, false])
                        .stick_to_bottom(true)
                        .show_themed(
                        ui,
                        |ui| {
                            if rx_text.is_empty() {
                                ui.label(
                                    RichText::new("— listening —")
                                        .monospace()
                                        .size(12.0)
                                        .color(Color32::from_gray(90)),
                                );
                            } else {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(&rx_text)
                                            .monospace()
                                            .size(12.5)
                                            .color(crate::theme::GREEN),
                                    )
                                    .wrap(),
                                );
                            }
                        },
                    );
                });
        });
        ui.add_space(gap);

        // TX input: already-sent characters are coloured green via a layouter.
        let prev = self.text_tx.clone();
        let sent = sent.min(prev.chars().count());
        let prefix: String = prev.chars().take(sent).collect();
        let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, wrap: f32| {
            let text = buf.as_str();
            let sent_byte =
                text.char_indices().nth(sent).map(|(i, _)| i).unwrap_or(text.len());
            let mut job = egui::text::LayoutJob::default();
            job.wrap.max_width = wrap;
            let mono = egui::FontId::monospace(13.0);
            if sent_byte > 0 {
                job.append(
                    &text[..sent_byte],
                    0.0,
                    egui::TextFormat { font_id: mono.clone(), color: crate::theme::GREEN, ..Default::default() },
                );
            }
            if sent_byte < text.len() {
                job.append(
                    &text[sent_byte..],
                    0.0,
                    egui::TextFormat { font_id: mono.clone(), color: crate::theme::TEXT_STRONG, ..Default::default() },
                );
            }
            ui.fonts_mut(|f| f.layout_job(job))
        };
        // Fixed-height box: the multiline TextEdit grows with content, so wrap it
        // in a bounded ScrollArea (stick-to-bottom) instead of letting it push the
        // buttons off the panel.
        let resp = ui
            .allocate_ui(egui::vec2(ui.available_width(), input_h), |ui| {
                egui::Frame::new()
                    .fill(crate::theme::ROW_BG)
                    .stroke(egui::Stroke::new(1.0, crate::theme::RED_DEEP))
                    .inner_margin(egui::Margin::symmetric(6, 4))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.set_min_height(ui.available_height());
                        egui::ScrollArea::vertical()
                            .id_salt("text-tx")
                            // Cap the height so the multiline scrolls internally
                            // instead of growing and pushing the buttons off-panel.
                            .max_height((input_h - 8.0).max(20.0))
                            .auto_shrink([false, false])
                            .stick_to_bottom(true)
                            .show_themed(ui, |ui| {
                                ui.add(
                                    egui::TextEdit::multiline(&mut self.text_tx)
                                        .layouter(&mut layouter)
                                        .frame(egui::Frame::NONE)
                                        .desired_width(f32::INFINITY)
                                        .hint_text("Type here to transmit…"),
                                )
                            })
                            .inner
                    })
                    .inner
            })
            .inner;
        if resp.changed() {
            // Protect the already-transmitted prefix from edits.
            if !self.text_tx.starts_with(&prefix) {
                self.text_tx = prev;
            }
            cmds.push(Command::DigiTxText(self.text_tx.clone()));
        }
        ui.add_space(gap);

        // Controls.
        ui.horizontal(|ui| {
            let label = if tx_on { "  TX ON  " } else { "   TX   " };
            if crate::chrome::chip_accent(
                ui,
                tx_on,
                RichText::new(label).size(14.0).strong(),
                crate::theme::PINK,
                Color32::WHITE,
            )
            .clicked()
            {
                cmds.push(Command::DigiTxActive(!tx_on));
            }
            if crate::chrome::chip_accent(
                ui,
                false,
                RichText::new(" CALL CQ ").size(13.0).strong(),
                crate::theme::GREEN,
                crate::theme::INK_ON_CYAN,
            )
            .clicked()
            {
                // Own the CQ text so the green sent-progress shows locally.
                let call = if my_call.is_empty() { "NOCALL".to_string() } else { my_call.clone() };
                let cq = format!("CQ CQ CQ DE {call} {call} {call} PSE K\n");
                cmds.push(Command::DigiAbortTx);
                self.text_tx = cq.clone();
                cmds.push(Command::DigiTxText(cq));
                cmds.push(Command::DigiTxActive(true));
            }
            if crate::chrome::chip(ui, false, " CLEAR ").clicked() {
                self.text_tx.clear();
                cmds.push(Command::DigiAbortTx);
                cmds.push(Command::DigiTxText(String::new()));
            }
        });
        // Visible padding below the buttons so they aren't flush with the edge.
        ui.add_space(bottom_pad);
    }

    /// Mode-specific parameter buttons for the continuous keyboard modes, shown
    /// inline in the panel header next to the tune buttons (moved here from the
    /// setup dialog). Edits the UI-owned config copy and pushes it on change.
    /// PSK has no parameters, so nothing is drawn.
    fn text_modem_params_row(&mut self, ui: &mut egui::Ui, cmds: &mut Vec<Command>) {
        let mode = self.state.rx[0].mode;
        if !matches!(mode, Mode::Rtty | Mode::Olivia | Mode::Thor) {
            return; // PSK (and anything else) has no per-mode settings
        }
        fn cap(ui: &mut egui::Ui, text: &str) {
            ui.label(RichText::new(text).size(10.5).strong().color(crate::theme::CYAN_DIM));
        }
        let cfg = &mut self.digi_cfg_edit;
        let mut changed = false;
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            match mode {
                Mode::Rtty => {
                    cap(ui, "Shift");
                    for s in [170.0f32, 425.0, 850.0] {
                        let sel = (cfg.rtty_shift_hz - s).abs() < 0.5;
                        if ui.selectable_label(sel, format!("{s:.0}")).clicked() {
                            cfg.rtty_shift_hz = s;
                            changed = true;
                        }
                    }
                    ui.add_space(8.0);
                    cap(ui, "Baud");
                    for b in [45.45f32, 50.0, 75.0] {
                        let sel = (cfg.rtty_baud - b).abs() < 0.5;
                        let lbl =
                            if (b - 45.45).abs() < 0.5 { "45".to_string() } else { format!("{b:.0}") };
                        if ui.selectable_label(sel, lbl).clicked() {
                            cfg.rtty_baud = b;
                            changed = true;
                        }
                    }
                }
                Mode::Olivia => {
                    cap(ui, "Tones");
                    for t in [2u8, 4, 8, 16, 32, 64] {
                        if ui.selectable_label(cfg.olivia_tones == t, t.to_string()).clicked() {
                            cfg.olivia_tones = t;
                            changed = true;
                        }
                    }
                    ui.add_space(8.0);
                    cap(ui, "BW");
                    for bw in [125.0f32, 250.0, 500.0, 1000.0, 2000.0] {
                        let sel = (cfg.olivia_bw_hz - bw).abs() < 0.5;
                        if ui.selectable_label(sel, format!("{bw:.0}")).clicked() {
                            cfg.olivia_bw_hz = bw;
                            changed = true;
                        }
                    }
                }
                Mode::Thor => {
                    cap(ui, "Mode");
                    for m in sdroxide_types::ThorMode::ALL {
                        if ui.selectable_label(cfg.thor_mode == m, m.label()).clicked() {
                            cfg.thor_mode = m;
                            changed = true;
                        }
                    }
                }
                _ => {}
            }
        });
        if changed {
            cmds.push(Command::SetDigiConfig(cfg.clone()));
        }
    }

    /// FSQ mode settings (speed + directed-message callsign), shown inline in the
    /// FSQ panel header next to the tune buttons.
    fn fsq_params_row(&mut self, ui: &mut egui::Ui, cmds: &mut Vec<Command>) {
        let cfg = &mut self.digi_cfg_edit;
        let mut changed = false;
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            ui.label(RichText::new("Speed").size(10.5).strong().color(crate::theme::CYAN_DIM));
            for b in [2.0f32, 3.0, 4.5, 6.0] {
                let sel = (cfg.fsq_baud - b).abs() < 0.05;
                let lbl = if (b - 4.5).abs() < 0.05 { "4.5".to_string() } else { format!("{b:.0}") };
                if ui.selectable_label(sel, lbl).clicked() {
                    cfg.fsq_baud = b;
                    changed = true;
                }
            }
            ui.add_space(8.0);
            ui.label(RichText::new("Call").size(10.5).strong().color(crate::theme::CYAN_DIM));
            if ui
                .add(egui::TextEdit::singleline(&mut cfg.fsq_call).desired_width(76.0))
                .on_hover_text("Callsign for directed (FSQCALL) messages; defaults to your callsign")
                .changed()
            {
                cfg.fsq_call = cfg.fsq_call.to_uppercase();
                changed = true;
            }
        });
        if changed {
            cmds.push(Command::SetDigiConfig(cfg.clone()));
        }
    }

    /// FSQ panel: the decoded stream + the directed (FSQCALL) layer — a heard
    /// list, a directed compose row (To: + message), and a contacts book.
    /// `panel_h` is the real bounded height (the frame reports an unbounded
    /// `available_height`).
    fn fsq_panel(&mut self, ui: &mut egui::Ui, cmds: &mut Vec<Command>, panel_h: f32) {
        let content_bottom = ui.cursor().top() + panel_h - 26.0;
        let status = self.digi_status.clone();
        let audio_hz = status.as_ref().map(|s| s.audio_hz).unwrap_or(1500.0);
        let transmitting = status.as_ref().map(|s| s.transmitting).unwrap_or(false);
        let text_rx = status.as_ref().map(|s| s.text_rx.clone()).unwrap_or_default();
        let heard = status.as_ref().map(|s| s.fsq_heard.clone()).unwrap_or_default();
        let messages = status.as_ref().map(|s| s.fsq_messages.clone()).unwrap_or_default();
        let my_call = {
            let c = &self.digi_cfg_edit;
            if !c.fsq_call.is_empty() { c.fsq_call.clone() } else { c.my_call.clone() }
        };

        // A picked image → transmit (the engine grayscales/scales it).
        if let Some(bytes) = self.fsq_img_inbox.lock().ok().and_then(|mut g| g.take()) {
            cmds.push(Command::DigiAbortTx);
            cmds.push(Command::DigiImageTx { png: bytes });
        }

        // Header row. Label matches the RTTY/PSK panels (11 pt cyan).
        ui.horizontal(|ui| {
            ui.label(RichText::new("FSQ").size(11.0).strong().color(crate::theme::CYAN));
            ui.label(RichText::new(format!("{audio_hz:.0} Hz")).monospace());
            if crate::chrome::chip(ui, false, "−").clicked() {
                cmds.push(Command::SetDigiAudioFreq((audio_hz - 10.0).clamp(200.0, 3500.0)));
            }
            if crate::chrome::chip(ui, false, "+").clicked() {
                cmds.push(Command::SetDigiAudioFreq((audio_hz + 10.0).clamp(200.0, 3500.0)));
            }
            // Mode settings inline next to the tune buttons (moved here from the
            // setup dialog): the FSQ speed and the directed-message callsign.
            self.fsq_params_row(ui, cmds);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if transmitting {
                    ui.label(RichText::new("● TX").color(crate::theme::PINK).strong());
                }
                if crate::chrome::chip(ui, self.fsq_show_contacts, "CONTACTS").clicked() {
                    self.fsq_show_contacts = !self.fsq_show_contacts;
                }
                self.digi_squelch_slider(ui, cmds);
            });
        });
        ui.add_space(4.0);
        // Everything below fits inside the (bounded) panel height. The left
        // column scrolls as a unit; the right column pins its compose controls to
        // the bottom so they're never clipped on a short panel.
        let avail_h = (content_bottom - ui.cursor().top()).max(80.0);
        ui.horizontal_top(|ui| {
            // ── Left: heard list + image, each in its own bounded scroll so the
            // heard list scrolls internally instead of pushing the ALLCALL/IMAGE
            // controls off-panel. Fixed room is reserved for the labels/buttons
            // between them; the split never overflows the column height. ──
            ui.vertical(|ui| {
                ui.set_width(150.0);
                let fixed_h = 96.0; // HEARD/IMAGE labels + ALLCALL + Send + separator
                let scrollable = (avail_h - fixed_h).max(44.0);
                let heard_h = scrollable * 0.62;
                let images_h = scrollable - heard_h;

                ui.label(RichText::new("HEARD").size(10.5).strong().color(crate::theme::CYAN_DIM));
                egui::ScrollArea::vertical()
                    .id_salt("fsq-heard")
                    .max_height(heard_h)
                    .auto_shrink([false, true])
                    .show_themed(ui, |ui| {
                        if heard.is_empty() {
                            ui.label(RichText::new("— none —").weak());
                        }
                        for call in &heard {
                            let sel = self.fsq_target.eq_ignore_ascii_case(call);
                            if ui.selectable_label(sel, RichText::new(call).monospace()).clicked() {
                                self.fsq_target = call.clone();
                            }
                        }
                    });
                ui.add_space(4.0);
                if crate::chrome::chip(ui, self.fsq_target.is_empty(), "ALLCALL").clicked() {
                    self.fsq_target.clear();
                }
                ui.separator();
                ui.label(RichText::new("IMAGE").size(10.5).strong().color(crate::theme::CYAN_DIM));
                if crate::chrome::chip(ui, false, "Send image…").clicked() {
                    pick_image(self.fsq_img_inbox.clone());
                }
                egui::ScrollArea::vertical()
                    .id_salt("fsq-images")
                    .max_height(images_h)
                    .auto_shrink([false, true])
                    .show_themed(ui, |ui| {
                        for tex in &self.fsq_rx_images {
                            ui.add(
                                egui::Image::new(tex)
                                    .fit_to_exact_size(egui::vec2(140.0, 105.0))
                                    .corner_radius(2.0),
                            );
                            ui.add_space(3.0);
                        }
                    });
            });

            ui.separator();

            // ── Right: RX stream (fills) + compose controls (pinned bottom) ──
            ui.vertical(|ui| {
                // Two control rows (To:/message/SEND, then CQ/? /CLEAR) + gaps.
                let controls_h = 74.0;
                let rx_h = (avail_h - controls_h).max(24.0);
                ui.allocate_ui(egui::vec2(ui.available_width(), rx_h), |ui| {
                    egui::Frame::new()
                        .fill(crate::theme::ROW_BG)
                        .stroke(egui::Stroke::new(1.0, crate::theme::RED_DEEP))
                        .inner_margin(6.0)
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.set_min_height(ui.available_height());
                            egui::ScrollArea::vertical()
                                .id_salt("fsq-rx")
                                .auto_shrink([false, false])
                                .stick_to_bottom(true)
                                .show_themed(ui, |ui| {
                                    if text_rx.is_empty() && messages.is_empty() {
                                        ui.label(RichText::new("— listening —").weak());
                                    }
                                    for m in messages.iter().filter(|m| m.to_me && !m.to.is_empty()) {
                                        ui.label(
                                            RichText::new(format!(
                                                "★ {} → {}: {}",
                                                m.from, m.to, m.text
                                            ))
                                            .color(crate::theme::CYAN)
                                            .monospace(),
                                        );
                                    }
                                    ui.label(
                                        RichText::new(&text_rx).monospace().color(crate::theme::GREEN),
                                    );
                                });
                        });
                });
                ui.add_space(4.0);
                let tgt = if self.fsq_target.is_empty() {
                    "ALLCALL".to_string()
                } else {
                    self.fsq_target.clone()
                };
                // Row 1: To: target + message input + SEND.
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("{tgt}:")).monospace().color(crate::theme::CYAN_DIM),
                    );
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.text_tx)
                            .desired_width((ui.available_width() - 62.0).max(60.0))
                            .hint_text("Message…"),
                    );
                    let send = crate::chrome::chip_accent(
                        ui,
                        false,
                        " SEND ",
                        crate::theme::PINK,
                        crate::theme::INK_ON_CYAN,
                    )
                    .clicked()
                        || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                    if send && !self.text_tx.trim().is_empty() {
                        let call = if my_call.is_empty() { "NOCALL" } else { &my_call };
                        let body = self.text_tx.trim();
                        let full = if self.fsq_target.is_empty() {
                            format!("{call}: {body}\n")
                        } else {
                            format!("{call}:{} {body}\n", self.fsq_target)
                        };
                        cmds.push(Command::DigiAbortTx);
                        cmds.push(Command::DigiTxText(full));
                        cmds.push(Command::DigiTxActive(true));
                        self.text_tx.clear();
                    }
                });
                // Row 2: CQ / ? heard / CLEAR.
                ui.horizontal(|ui| {
                    if crate::chrome::chip(ui, false, " CALL CQ ").clicked() {
                        cmds.push(Command::DigiCallCq);
                    }
                    if !self.fsq_target.is_empty()
                        && crate::chrome::chip(ui, false, " ? heard ").clicked()
                    {
                        let call = if my_call.is_empty() { "NOCALL" } else { &my_call };
                        let full = format!("{call}:{}?\n", self.fsq_target);
                        cmds.push(Command::DigiAbortTx);
                        cmds.push(Command::DigiTxText(full));
                        cmds.push(Command::DigiTxActive(true));
                    }
                    if crate::chrome::chip(ui, false, " CLEAR ").clicked() {
                        self.text_tx.clear();
                        cmds.push(Command::DigiAbortTx);
                    }
                });
            });
        });

        self.fsq_contacts_window(ui.ctx());
    }

    /// Editable FSQ contacts book (add / select-as-target / delete).
    fn fsq_contacts_window(&mut self, ctx: &egui::Context) {
        let mut open = self.fsq_show_contacts;
        let mut changed = false;
        let mut set_target: Option<String> = None;
        egui::Window::new("FSQ Contacts")
            .open(&mut open)
            .frame(crate::chrome::window_frame())
            .resizable(false)
            .default_width(320.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.fsq_new_contact)
                            .desired_width(140.0)
                            .hint_text("callsign"),
                    );
                    let can_add = !self.fsq_new_contact.trim().is_empty();
                    if ui.add_enabled(can_add, egui::Button::new("Add")).clicked() {
                        let id = self.fsq_contacts.iter().map(|c| c.id).max().unwrap_or(0) + 1;
                        self.fsq_contacts.push(sdroxide_types::FsqContact {
                            id,
                            call: self.fsq_new_contact.trim().to_uppercase(),
                            name: String::new(),
                            note: String::new(),
                        });
                        self.fsq_new_contact.clear();
                        changed = true;
                    }
                });
                ui.separator();
                let mut to_delete: Option<u64> = None;
                egui::ScrollArea::vertical().max_height(260.0).show_themed(ui, |ui| {
                    for c in &mut self.fsq_contacts {
                        ui.horizontal(|ui| {
                            if ui.button("TO").clicked() {
                                set_target = Some(c.call.clone());
                            }
                            ui.label(RichText::new(&c.call).monospace().strong());
                            if ui.add(egui::TextEdit::singleline(&mut c.name).hint_text("name").desired_width(120.0)).changed() {
                                changed = true;
                            }
                            if crate::chrome::chip_accent(ui, false, "DEL", crate::theme::PINK, crate::theme::INK_ON_CYAN).clicked() {
                                to_delete = Some(c.id);
                            }
                        });
                    }
                });
                if let Some(id) = to_delete {
                    self.fsq_contacts.retain(|c| c.id != id);
                    changed = true;
                }
            });
        if let Some(t) = set_target {
            self.fsq_target = t;
            self.fsq_show_contacts = false;
        }
        if changed {
            fsq_save_contacts(&self.fsq_contacts);
        }
        self.fsq_show_contacts = open;
    }

    /// Own-call / grid / message-template editor (and RTTY parameters).
    fn digi_settings_window(&mut self, ctx: &egui::Context, cmds: &mut Vec<Command>) {
        let mut open = self.show_digi_settings;
        let mode = self.state.rx[0].mode;
        // Per-mode parameters (RTTY/Olivia/THOR/FSQ) now live in each panel's
        // header, so this dialog only carries the shared identity + FT8/FT4
        // message templates.
        let title = if mode.is_text_modem() {
            format!("{} Setup", mode.label())
        } else {
            "FT8 / FT4 Setup".to_string()
        };
        let resp = egui::Window::new(title)
            .open(&mut open)
            .frame(crate::chrome::window_frame())
            .resizable(false)
            .default_width(420.0)
            .show(ctx, |ui| {
                // Edit the UI-owned copy so keystrokes aren't clobbered by the
                // engine's status echo; persist on any change.
                let cfg = &mut self.digi_cfg_edit;
                let mut changed = false;
                egui::Grid::new("digi-cfg").num_columns(2).show(ui, |ui| {
                    ui.label("My callsign");
                    if ui.text_edit_singleline(&mut cfg.my_call).changed() {
                        cfg.my_call = cfg.my_call.to_uppercase();
                        changed = true;
                    }
                    ui.end_row();
                    ui.label("My grid");
                    if ui.text_edit_singleline(&mut cfg.my_grid).changed() {
                        changed = true;
                    }
                    ui.end_row();
                    ui.label("TX period");
                    ui.horizontal(|ui| {
                        changed |= ui.selectable_value(&mut cfg.tx_even, true, "Even").changed();
                        changed |= ui.selectable_value(&mut cfg.tx_even, false, "Odd").changed();
                    });
                    ui.end_row();
                    ui.label("Auto-sequence");
                    changed |= ui.checkbox(&mut cfg.auto_seq, "").changed();
                    ui.end_row();
                    ui.label("TX watchdog");
                    changed |= ui
                        .add(
                            egui::DragValue::new(&mut cfg.tx_watchdog_min)
                                .range(0..=60)
                                .suffix(" min"),
                        )
                        .on_hover_text(
                            "Stop transmitting after this long with no reply and no action \
                             from you. 0 disables it.",
                        )
                        .changed();
                    ui.end_row();
                    ui.label("Give up after");
                    changed |= ui
                        .add(
                            egui::DragValue::new(&mut cfg.max_tx_repeats)
                                .range(0..=30)
                                .suffix(" calls"),
                        )
                        .on_hover_text(
                            "Unanswered calls to one station before moving on. Calling CQ is \
                             exempt. 0 disables it.",
                        )
                        .changed();
                    ui.end_row();
                });
                ui.separator();
                ui.label(
                    RichText::new("Message templates  {MYCALL} {MYGRID} {DX} {REPORT}")
                        .size(10.5)
                        .color(Color32::from_gray(150)),
                );
                egui::Grid::new("digi-msgs").num_columns(2).show(ui, |ui| {
                    for (label, field) in [
                        ("CQ", &mut cfg.msg_cq),
                        ("Grid", &mut cfg.msg_grid),
                        ("Report", &mut cfg.msg_report),
                        ("R+Report", &mut cfg.msg_rreport),
                        ("RR73", &mut cfg.msg_rr73),
                        ("73", &mut cfg.msg_73),
                    ] {
                        ui.label(label);
                        changed |= ui.text_edit_singleline(field).changed();
                        ui.end_row();
                    }
                });
                if changed {
                    cmds.push(Command::SetDigiConfig(cfg.clone()));
                }
            });
        if let Some(r) = &resp {
            crate::chrome::paint_window_border(ctx, &r.response);
        }
        self.show_digi_settings = open;
    }

    fn memories_window(&mut self, ctx: &egui::Context, cmds: &mut Vec<Command>) {
        let mut open = self.show_memories;
        let resp = egui::Window::new("Memories")
            .open(&mut open)
            .frame(crate::chrome::window_frame())
            .resizable(true)
            .default_width(340.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut self.mem_name);
                    let name_ok = !self.mem_name.trim().is_empty();
                    if ui.add_enabled(name_ok, egui::Button::new("Store")).clicked() {
                        cmds.push(Command::StoreMemory { name: self.mem_name.trim().to_string() });
                        self.mem_name.clear();
                    }
                });
                ui.separator();
                if self.memories.is_empty() {
                    ui.label(RichText::new("no memories yet").color(Color32::from_gray(120)));
                }
                for m in &self.memories {
                    ui.horizontal(|ui| {
                        if crate::chrome::chip(ui, false, "RCL").on_hover_text("Recall").clicked() {
                            cmds.push(Command::RecallMemory(m.id));
                        }
                        ui.label(
                            RichText::new(format!(
                                "{:<12} {:>12.6} MHz  {}",
                                m.name,
                                m.freq_hz / 1e6,
                                m.mode.label()
                            ))
                            .monospace(),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if crate::chrome::chip_accent(
                                ui,
                                false,
                                RichText::new("DEL").size(11.0),
                                crate::theme::PINK,
                                Color32::WHITE,
                            )
                            .on_hover_text("Delete")
                            .clicked()
                            {
                                cmds.push(Command::DeleteMemory(m.id));
                            }
                        });
                    });
                }
            });
        if let Some(r) = &resp {
            crate::chrome::paint_window_border(ctx, &r.response);
        }
        self.show_memories = open;
    }

    /// The voice keyer: ten recorded messages with record / transmit / erase
    /// per slot.
    ///
    /// Everything the window shows comes from the engine (it owns the
    /// recordings and the transmitter), so the buttons only ever send commands
    /// — there is no local latch that could disagree with what is on the air.
    fn voice_window(&mut self, ctx: &egui::Context, cmds: &mut Vec<Command>) {
        // Entering a digital mode other than RADE takes the feature away; the
        // window goes with it rather than sitting there doing nothing.
        if !self.state.rx[0].mode.allows_voice_keyer() {
            self.show_voice = false;
            return;
        }
        let mut open = self.show_voice;
        let recording = self.voice.recording;
        let playing = self.voice.playing;
        let previewing = self.voice.previewing;
        let pos = self.voice.position_s;
        let max_len = self.voice.max_len_s;
        // TUNE holds the transmitter at the tune level, so a message would go
        // nowhere; the engine refuses, and the buttons say so up front.
        let tuning = self.state.tx.tune;
        let slots: Vec<sdroxide_types::VoiceSlotInfo> = self.voice.slots.clone();

        let resp = egui::Window::new("Voice keyer")
            .open(&mut open)
            .frame(crate::chrome::window_frame())
            .resizable(false)
            // `min_width` as well as `default_width`: the default only applies
            // the first time the window is ever shown, and egui persists its
            // size — without the minimum, a build that shipped a narrower
            // window would keep squeezing the slot-name fields forever.
            .default_width(600.0)
            .min_width(600.0)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(
                        "REC records from your microphone, PLAY lets you listen to what you \
                         recorded, TX puts it on the air — as does a numpad key, a MIDI pad, \
                         or rigctld's send_voice_mem.",
                    )
                    .weak()
                    .size(11.5),
                );
                ui.add_space(6.0);
                egui::Grid::new("voice-grid").num_columns(6).spacing([8.0, 6.0]).striped(true).show(
                    ui,
                    |ui| {
                        for (i, slot) in slots.iter().enumerate() {
                            let is_rec = recording == Some(i as u8);
                            let is_play = playing == Some(i as u8);
                            let is_prev = previewing == Some(i as u8);

                            ui.label(
                                RichText::new(format!("{:>2}", i + 1))
                                    .monospace()
                                    .color(crate::theme::CYAN_DIM),
                            );

                            // The slot label. Only the row being typed into is
                            // UI-owned; every other row shows the engine's copy.
                            let mut text = match &self.voice_name_edit {
                                Some((row, s)) if *row == i => s.clone(),
                                _ => slot.name.clone(),
                            };
                            // `add_sized`, not `desired_width`: inside a Grid a
                            // desired width is clamped by the column width egui
                            // measured (and persisted) last frame, so a field
                            // that once came up narrow would stay narrow.
                            let edit = ui.add_sized(
                                [190.0, 20.0],
                                egui::TextEdit::singleline(&mut text)
                                    .hint_text(format!("Slot {}", i + 1)),
                            );
                            if edit.changed() {
                                self.voice_name_edit = Some((i, text.clone()));
                            }
                            if edit.lost_focus()
                                && let Some((row, name)) = self.voice_name_edit.take()
                                && row == i
                            {
                                cmds.push(Command::VoiceRename { slot: i as u8, name });
                            }

                            // REC — starts/stops recording this slot. Refused
                            // while the transmitter is up (same microphone).
                            let busy_elsewhere = (recording.is_some() && !is_rec)
                                || playing.is_some()
                                || previewing.is_some()
                                || self.state.tx.ptt
                                || tuning;
                            let rec = ui
                                .add_enabled_ui(!busy_elsewhere, |ui| {
                                    crate::chrome::chip_accent(
                                        ui,
                                        is_rec,
                                        RichText::new("REC").size(11.5),
                                        crate::theme::PINK,
                                        Color32::WHITE,
                                    )
                                })
                                .inner
                                .on_hover_text(if is_rec {
                                    "Stop and store".to_string()
                                } else {
                                    format!("Record from the microphone (up to {max_len:.0} s)")
                                });
                            if rec.clicked() {
                                cmds.push(Command::VoiceRecord(
                                    if is_rec { None } else { Some(i as u8) },
                                ));
                            }

                            // PLAY — listen to the message locally. Nothing goes
                            // on the air, so this is safe to press any time the
                            // receiver is running.
                            let can_prev = !slot.is_empty()
                                && recording.is_none()
                                && !self.state.tx.ptt
                                && !tuning
                                && (is_prev || previewing.is_none());
                            let prev = ui
                                .add_enabled_ui(can_prev || is_prev, |ui| {
                                    crate::chrome::chip(
                                        ui,
                                        is_prev,
                                        RichText::new(if is_prev { "STOP" } else { "PLAY" })
                                            .size(11.5),
                                    )
                                })
                                .inner
                                .on_hover_text(if is_prev {
                                    "Stop listening"
                                } else if slot.is_empty() {
                                    "Nothing recorded in this slot"
                                } else if self.state.tx.ptt || tuning {
                                    "Not while transmitting"
                                } else {
                                    "Listen to this message — nothing is transmitted"
                                });
                            if prev.clicked() {
                                cmds.push(if is_prev {
                                    Command::VoicePreview(None)
                                } else {
                                    Command::VoicePreview(Some(i as u8))
                                });
                            }

                            // TX — puts the message on the air.
                            let can_play = !slot.is_empty()
                                && recording.is_none()
                                && !tuning
                                && (is_play || playing.is_none());
                            let play = ui
                                .add_enabled_ui(can_play || is_play, |ui| {
                                    crate::chrome::chip_accent(
                                        ui,
                                        is_play,
                                        RichText::new(if is_play { "STOP" } else { "TX" })
                                            .size(11.5),
                                        crate::theme::PINK,
                                        Color32::WHITE,
                                    )
                                })
                                .inner
                                .on_hover_text(if is_play {
                                    "Stop transmitting"
                                } else if slot.is_empty() {
                                    "Nothing recorded in this slot"
                                } else if tuning {
                                    "TUNE is active — switch it off first"
                                } else {
                                    "Transmit this message"
                                });
                            if play.clicked() {
                                cmds.push(if is_play {
                                    Command::VoicePlay(None)
                                } else {
                                    Command::VoicePlay(Some(i as u8))
                                });
                            }

                            // Length, or the running position of whichever of
                            // record / listen / transmit this row owns.
                            ui.horizontal(|ui| {
                                let (text, colour) = if is_rec {
                                    (format!("● {pos:.1} s"), crate::theme::PINK)
                                } else if is_play {
                                    (
                                        format!("▶ {pos:.1} / {:.1} s", slot.len_s),
                                        crate::theme::PINK,
                                    )
                                } else if is_prev {
                                    (
                                        format!("▶ {pos:.1} / {:.1} s", slot.len_s),
                                        crate::theme::CYAN,
                                    )
                                } else if slot.is_empty() {
                                    ("—".to_string(), Color32::from_gray(110))
                                } else {
                                    (format!("{:.1} s", slot.len_s), Color32::from_gray(170))
                                };
                                ui.add_sized(
                                    [88.0, 18.0],
                                    egui::Label::new(
                                        RichText::new(text).monospace().size(11.5).color(colour),
                                    )
                                    .selectable(false),
                                );
                                let erasable =
                                    !slot.is_empty() && !is_rec && !is_play && !is_prev;
                                if ui
                                    .add_enabled_ui(erasable, |ui| {
                                        crate::chrome::chip_accent(
                                            ui,
                                            false,
                                            RichText::new("DEL").size(11.0),
                                            crate::theme::PINK,
                                            Color32::WHITE,
                                        )
                                    })
                                    .inner
                                    .on_hover_text("Erase this recording")
                                    .clicked()
                                {
                                    cmds.push(Command::VoiceClear(i as u8));
                                }
                            });
                            ui.end_row();
                        }
                    },
                );

                if self.state.rx[0].mode.is_rade() {
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(
                            "RADE: the message is encoded by the digital-voice codec, \
                             exactly as a live over would be.",
                        )
                        .weak()
                        .size(11.0),
                    );
                }
            });
        if let Some(r) = &resp {
            crate::chrome::paint_window_border(ctx, &r.response);
        }
        // Keep the position readout moving while something is running; the app
        // otherwise idles between spectrum frames.
        if self.voice.busy() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        self.show_voice = open;
    }

    /// Tune the active VFO onto a spot (CW dialed a pitch below, so it lands in
    /// the CW passband), set its mode, and open a pre-filled log entry.
    fn select_spot(&mut self, spot: &Spot, cmds: &mut Vec<Command>) {
        match spot.radio_mode() {
            Some(Mode::Cw) => {
                let (lo, hi) = Mode::Cw.default_filter();
                let pitch = ((lo + hi) * 0.5) as f64;
                cmds.push(Command::SetVfo { vfo: self.state.active_vfo, hz: spot.freq_hz - pitch });
                cmds.push(Command::SetMode { rx: RxId::Main, mode: Mode::Cw });
            }
            Some(m) => {
                cmds.push(Command::SetVfo { vfo: self.state.active_vfo, hz: spot.freq_hz });
                cmds.push(Command::SetMode { rx: RxId::Main, mode: m });
            }
            None => cmds.push(Command::SetVfo { vfo: self.state.active_vfo, hz: spot.freq_hz }),
        }
        self.prefill_from_spot(spot);
    }

    /// The live-spots window: source filters, a click-to-tune list of current
    /// DX-cluster / POTA / SOTA / PSK-Reporter spots, and the feed status line.
    fn spots_window(&mut self, ctx: &egui::Context, cmds: &mut Vec<Command>) {
        let worked_entities = self.worked_entities().clone();
        let mut open = self.show_spots;
        let mut clicked: Option<Spot> = None;
        let mut open_setup = false;
        let now = now_unix();
        let spots = self.spots.clone();
        let labels = [
            (SpotKind::DxCluster, "DX"),
            (SpotKind::Pota, "POTA"),
            (SpotKind::Sota, "SOTA"),
            (SpotKind::PskReporter, "PSK"),
            (SpotKind::FreeDv, "FREEDV"),
        ];
        let resp = egui::Window::new("SPOTS")
            .open(&mut open)
            .frame(crate::chrome::window_frame())
            .resizable(true)
            .default_width(580.0)
            .default_height(480.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    for (i, (_, label)) in labels.iter().enumerate() {
                        if crate::chrome::chip(ui, self.spot_kinds_shown[i], *label).clicked() {
                            self.spot_kinds_shown[i] = !self.spot_kinds_shown[i];
                        }
                    }
                    if crate::chrome::chip(ui, self.spot_in_view_only, "IN VIEW")
                        .on_hover_text("Only spots inside the panadapter span")
                        .clicked()
                    {
                        self.spot_in_view_only = !self.spot_in_view_only;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if crate::chrome::chip(ui, false, "⚙ SETUP")
                            .on_hover_text("Feeds, lookup & upload settings")
                            .clicked()
                        {
                            open_setup = true;
                        }
                    });
                });
                if let Some(s) = &self.net_status {
                    ui.label(RichText::new(s).size(11.0).color(Color32::from_gray(150)));
                }
                ui.separator();
                egui::ScrollArea::vertical().auto_shrink([false, false]).show_themed(ui, |ui| {
                    let mut shown = 0usize;
                    for s in &spots {
                        if !self.spot_kinds_shown[spot_kind_index(s.kind)] {
                            continue;
                        }
                        if self.spot_in_view_only
                            && !(self.view.view_lo_hz..=self.view.view_hi_hz).contains(&s.freq_hz)
                        {
                            continue;
                        }
                        let needed = sdroxide_types::entity_name(&s.call)
                            .map(|n| !worked_entities.contains(n))
                            .unwrap_or(false);
                        if spot_row(ui, s, now, needed).clicked() {
                            clicked = Some(s.clone());
                        }
                        shown += 1;
                    }
                    if shown == 0 {
                        ui.add_space(8.0);
                        ui.label(
                            RichText::new("no spots — enable a feed in ⚙ SETUP")
                                .color(Color32::from_gray(120)),
                        );
                    }
                });
            });
        if let Some(r) = &resp {
            crate::chrome::paint_window_border(ctx, &r.response);
        }
        self.show_spots = open;
        if open_setup {
            // Open the main Settings dialog on the Spots tab.
            self.show_settings = true;
            self.settings_tab = SettingsTab::Spots;
        }
        if let Some(s) = clicked {
            self.select_spot(&s, cmds);
        }
    }

    /// The logbook overlay: a session-grouped list of all QSOs (digital and
    /// manual), with add / edit / delete and ADIF/TXT export.
    fn logbook_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_logbook;
        let resp = egui::Window::new("LOGBOOK")
            .open(&mut open)
            .frame(crate::chrome::window_frame())
            .resizable(true)
            .default_width(720.0)
            .default_height(560.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let adding = self.log_edit.as_ref().is_some_and(|f| f.id == 0);
                    if crate::chrome::chip(ui, adding, "+ NEW ENTRY").clicked() {
                        let freq = self.state.rx_freq_hz();
                        let mode = self.state.rx[0].mode.label();
                        self.log_edit = Some(LogEditForm::new_entry(now_unix(), freq, mode));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let have = !self.qso_log.is_empty();
                        ui.add_enabled_ui(have, |ui| {
                            if crate::chrome::chip(ui, false, "TXT").clicked() {
                                let txt = sdroxide_types::qso_log_to_text(&self.qso_log);
                                crate::download::save("sdroxide-log.txt", txt.as_bytes());
                            }
                            if crate::chrome::chip(ui, false, "ADIF").clicked() {
                                let adif = sdroxide_types::qso_log_to_adif(&self.qso_log);
                                crate::download::save("sdroxide-log.adi", adif.as_bytes());
                            }
                        });
                        #[cfg(not(target_arch = "wasm32"))]
                        if crate::chrome::chip(ui, false, "IMPORT")
                            .on_hover_text("Import QSOs from an ADIF (.adi) file")
                            .clicked()
                        {
                            crate::download::load_text("ADIF", "adi", self.adif_import_inbox.clone());
                        }
                        ui.label(
                            RichText::new(format!("{} QSO", self.qso_log.len()))
                                .size(11.0)
                                .color(Color32::from_gray(150)),
                        );
                    });
                });
                if self.log_edit.is_some() {
                    ui.add_space(4.0);
                    self.log_entry_form(ui);
                }
                ui.separator();
                egui::ScrollArea::vertical().auto_shrink([false, false]).show_themed(ui, |ui| {
                    self.log_list(ui);
                });
            });
        if let Some(r) = &resp {
            crate::chrome::paint_window_border(ctx, &r.response);
        }
        self.show_logbook = open;
    }

    /// The new/edit entry form (shown inside the logbook when active).
    fn log_entry_form(&mut self, ui: &mut egui::Ui) {
        if self.log_edit.is_none() {
            return;
        }
        let mut action = 0u8; // 1 = save, 2 = cancel
        let mut set_now = false;
        // "Worked before" (same call + band) — computed before the mutable
        // borrow of the form below, against the current log.
        let (dupe, dupe_band) = {
            let f = self.log_edit.as_ref().unwrap();
            let freq_hz = f.freq_mhz.trim().parse::<f64>().ok().map(|m| m * 1e6).unwrap_or(0.0);
            let band =
                if freq_hz > 0.0 { sdroxide_types::adif_band(freq_hz).to_string() } else { String::new() };
            let dupe = !band.is_empty()
                && !f.call.trim().is_empty()
                && sdroxide_types::worked_before(&self.qso_log, f.call.trim(), &band, "", f.id);
            (dupe, band)
        };
        let auto_lookup = self.net_cfg_edit.auto_lookup;
        let has_provider = self.net_cfg_edit.lookup_provider != LookupProvider::None;
        let mut lookup_call: Option<String> = None;
        {
            let f = self.log_edit.as_mut().unwrap();
            egui::Frame::new()
                .fill(crate::theme::ROW_BG)
                .stroke(egui::Stroke::new(1.0, crate::theme::RED_DEEP))
                .inner_margin(egui::Margin::same(9))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(if f.id == 0 { "NEW QSO" } else { "EDIT QSO" })
                                .size(11.0)
                                .strong()
                                .color(crate::theme::CYAN),
                        );
                        if dupe {
                            ui.label(
                                RichText::new(format!("⚠ WORKED BEFORE ({dupe_band})"))
                                    .size(11.0)
                                    .strong()
                                    .color(crate::theme::PINK),
                            );
                        }
                    });
                    ui.add_space(4.0);
                    // Horizontal rows (not a Grid) so each field keeps its
                    // explicit width — a Grid redistributes column widths and
                    // squashes the narrow-looking ones.
                    let lbl = |ui: &mut egui::Ui, text: &str| {
                        let (rect, _) =
                            ui.allocate_exact_size(egui::vec2(72.0, 24.0), egui::Sense::hover());
                        ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(
                            egui::Layout::left_to_right(egui::Align::Center),
                        ))
                        .label(text);
                    };
                    let field = |ui: &mut egui::Ui, w: f32, s: &mut String| {
                        ui.add(egui::TextEdit::singleline(s).desired_width(w));
                    };
                    ui.horizontal(|ui| {
                        lbl(ui, "Call");
                        let cr = ui.add(egui::TextEdit::singleline(&mut f.call).desired_width(150.0));
                        if has_provider
                            && crate::chrome::chip(ui, false, "LOOKUP")
                                .on_hover_text("Look up name / QTH / grid")
                                .clicked()
                            && !f.call.trim().is_empty()
                        {
                            lookup_call = Some(f.call.trim().to_string());
                        }
                        lbl(ui, "Grid");
                        field(ui, 110.0, &mut f.grid);
                        // Auto-lookup when the call field loses focus.
                        if cr.lost_focus()
                            && auto_lookup
                            && has_provider
                            && !f.call.trim().is_empty()
                        {
                            lookup_call = Some(f.call.trim().to_string());
                        }
                    });
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        lbl(ui, "Freq MHz");
                        field(ui, 150.0, &mut f.freq_mhz);
                        lbl(ui, "Mode");
                        field(ui, 120.0, &mut f.mode);
                    });
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        lbl(ui, "RST sent");
                        field(ui, 150.0, &mut f.rst_sent);
                        lbl(ui, "RST rcvd");
                        field(ui, 120.0, &mut f.rst_rcvd);
                    });
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        lbl(ui, "Name");
                        field(ui, 150.0, &mut f.name);
                        lbl(ui, "QTH");
                        field(ui, 120.0, &mut f.qth);
                    });
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        lbl(ui, "State");
                        field(ui, 150.0, &mut f.state);
                        lbl(ui, "Country");
                        field(ui, 120.0, &mut f.country);
                    });
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        lbl(ui, "Date UTC");
                        field(ui, 150.0, &mut f.date);
                        lbl(ui, "Time");
                        field(ui, 90.0, &mut f.time);
                        if crate::chrome::chip(ui, false, "NOW").clicked() {
                            set_now = true;
                        }
                    });
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        lbl(ui, "Pwr W");
                        field(ui, 60.0, &mut f.tx_pwr);
                        lbl(ui, "Contest");
                        field(ui, 96.0, &mut f.contest_id);
                        lbl(ui, "S# sent");
                        field(ui, 56.0, &mut f.stx);
                        lbl(ui, "S# rcvd");
                        field(ui, 56.0, &mut f.srx);
                    });
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        lbl(ui, "Comment");
                        field(ui, 500.0, &mut f.comment);
                    });
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if crate::chrome::chip_accent(
                            ui,
                            false,
                            RichText::new(" SAVE ").strong(),
                            crate::theme::GREEN,
                            crate::theme::INK_ON_CYAN,
                        )
                        .clicked()
                        {
                            action = 1;
                        }
                        if crate::chrome::chip(ui, false, "CANCEL").clicked() {
                            action = 2;
                        }
                    });
                });
            if set_now {
                let (y, mo, d, h, mi, _) = sdroxide_types::utc_ymd_hms(now_unix());
                f.date = format!("{y:04}-{mo:02}-{d:02}");
                f.time = format!("{h:02}:{mi:02}");
            }
        }
        if let Some(c) = lookup_call {
            self.pending_lookups.push(c);
        }
        match action {
            1 => {
                let (mc, mg) =
                    (self.digi_cfg_edit.my_call.clone(), self.digi_cfg_edit.my_grid.clone());
                if let Some(f) = self.log_edit.take() {
                    if let Some(rec) = f.to_record(&mc, &mg) {
                        if rec.id == 0 {
                            let mut rec = rec;
                            rec.id = self.next_log_id();
                            self.qso_log.push(rec);
                        } else if let Some(e) = self.qso_log.iter_mut().find(|q| q.id == rec.id) {
                            *e = rec;
                        }
                        persist_qso_log(&self.qso_log);
                    } else {
                        // Empty callsign — keep the form open for correction.
                        self.log_edit = Some(f);
                    }
                }
            }
            2 => self.log_edit = None,
            _ => {}
        }
    }

    /// The QSO list, grouped into daily sessions (newest first).
    fn log_list(&mut self, ui: &mut egui::Ui) {
        if self.qso_log.is_empty() {
            ui.add_space(8.0);
            ui.label(
                RichText::new("no QSOs yet — run FT8/FT4 or add a manual entry")
                    .color(Color32::from_gray(120)),
            );
            return;
        }
        let mut order: Vec<usize> = (0..self.qso_log.len()).collect();
        order.sort_by(|&a, &b| self.qso_log[b].start_utc.cmp(&self.qso_log[a].start_utc));

        let mut to_edit: Option<u64> = None;
        let mut to_delete: Option<u64> = None;
        let mut to_upload: Option<u64> = None;
        // Which targets have credentials, so the per-QSO upload button is only
        // offered when it can do something.
        let up_targets = configured_upload_targets(&self.net_cfg_edit);

        let mut i = 0;
        while i < order.len() {
            let day = date_str(self.qso_log[order[i]].start_utc);
            let mut j = i;
            while j < order.len() && date_str(self.qso_log[order[j]].start_utc) == day {
                j += 1;
            }
            let group = &order[i..j];
            let newest = self.qso_log[group[0]].start_utc;
            let oldest = self.qso_log[group[group.len() - 1]].start_utc;
            // Session header.
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(&day).size(12.0).strong().color(crate::theme::CYAN));
                ui.label(
                    RichText::new(format!(
                        "{}–{} UTC · {} QSO",
                        time_str(oldest),
                        time_str(newest),
                        group.len()
                    ))
                    .size(10.5)
                    .color(Color32::from_gray(130)),
                );
            });
            ui.add_space(2.0);
            for &idx in group {
                let r = &self.qso_log[idx];
                let inner = egui::Frame::new()
                    .fill(crate::theme::ROW_BG)
                    .inner_margin(egui::Margin { left: 10, right: 6, top: 5, bottom: 5 })
                    .show(ui, |ui| {
                        ui.set_min_height(22.0);
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            let col = |ui: &mut egui::Ui, w: f32, lbl: egui::Label| {
                                let (rect, _) = ui
                                    .allocate_exact_size(egui::vec2(w, 20.0), egui::Sense::hover());
                                let mut c = ui.new_child(
                                    egui::UiBuilder::new().max_rect(rect).layout(
                                        egui::Layout::left_to_right(egui::Align::Center),
                                    ),
                                );
                                c.add(lbl);
                            };
                            let gray = Color32::from_gray(150);
                            col(
                                ui,
                                40.0,
                                egui::Label::new(
                                    RichText::new(time_str(r.start_utc)).monospace().size(12.0).color(gray),
                                ),
                            );
                            col(
                                ui,
                                92.0,
                                egui::Label::new(
                                    RichText::new(&r.call)
                                        .size(14.0)
                                        .strong()
                                        .color(crate::theme::TEXT_STRONG),
                                )
                                .truncate(),
                            );
                            col(
                                ui,
                                42.0,
                                egui::Label::new(
                                    RichText::new(&r.band).monospace().size(11.5).color(gray),
                                ),
                            );
                            col(
                                ui,
                                48.0,
                                egui::Label::new(
                                    RichText::new(&r.mode).monospace().size(11.5).color(gray),
                                ),
                            );
                            let rst = format!(
                                "{}/{}",
                                r.rst_sent.map(|v| v.to_string()).unwrap_or_else(|| "–".into()),
                                r.rst_rcvd.map(|v| v.to_string()).unwrap_or_else(|| "–".into()),
                            );
                            col(
                                ui,
                                72.0,
                                egui::Label::new(RichText::new(rst).monospace().size(11.5).color(gray)),
                            );
                            col(
                                ui,
                                48.0,
                                egui::Label::new(
                                    RichText::new(r.grid.as_deref().unwrap_or(""))
                                        .monospace()
                                        .size(11.5)
                                        .color(crate::theme::CYAN_DIM),
                                ),
                            );
                            // QSL / confirmation status: green ✓ when confirmed,
                            // dim ↑ when uploaded-but-unconfirmed, else blank.
                            let (qsl_txt, qsl_col) = if r.is_confirmed() {
                                ("✓", crate::theme::GREEN)
                            } else if r.lotw_sent || r.eqsl_sent || r.qrz_sent || r.clublog_sent {
                                ("↑", Color32::from_gray(140))
                            } else {
                                ("", gray)
                            };
                            let mut qsl_tip = String::new();
                            for (on, name) in [
                                (r.lotw_rcvd, "LoTW ✓"),
                                (r.eqsl_rcvd, "eQSL ✓"),
                                (r.qsl_rcvd, "card ✓"),
                                (r.lotw_sent, "LoTW ↑"),
                                (r.eqsl_sent, "eQSL ↑"),
                                (r.qrz_sent, "QRZ ↑"),
                                (r.clublog_sent, "Club Log ↑"),
                            ] {
                                if on {
                                    if !qsl_tip.is_empty() {
                                        qsl_tip.push_str(", ");
                                    }
                                    qsl_tip.push_str(name);
                                }
                            }
                            {
                                let (rect, _) = ui.allocate_exact_size(
                                    egui::vec2(16.0, 20.0),
                                    egui::Sense::hover(),
                                );
                                let mut c = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(
                                    egui::Layout::left_to_right(egui::Align::Center),
                                ));
                                let resp = c.add(egui::Label::new(
                                    RichText::new(qsl_txt).size(13.0).strong().color(qsl_col),
                                ));
                                if !qsl_tip.is_empty() {
                                    resp.on_hover_text(qsl_tip);
                                }
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if crate::chrome::chip_accent(
                                        ui,
                                        false,
                                        RichText::new("DEL").size(11.0),
                                        crate::theme::PINK,
                                        Color32::WHITE,
                                    )
                                    .on_hover_text("Delete this entry")
                                    .clicked()
                                    {
                                        to_delete = Some(r.id);
                                    }
                                    if crate::chrome::chip(ui, false, RichText::new("EDIT").size(11.0))
                                        .clicked()
                                    {
                                        to_edit = Some(r.id);
                                    }
                                    if !up_targets.is_empty()
                                        && crate::chrome::chip(ui, false, RichText::new("UP").size(11.0))
                                            .on_hover_text("Upload this QSO to configured logs")
                                            .clicked()
                                    {
                                        to_upload = Some(r.id);
                                    }
                                    if !r.comment.is_empty() {
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(&r.comment)
                                                    .size(11.5)
                                                    .color(Color32::from_gray(120)),
                                            )
                                            .truncate(),
                                        );
                                    }
                                },
                            );
                        });
                    });
                let rr = inner.response.rect;
                ui.painter().rect_filled(
                    egui::Rect::from_min_max(rr.left_top(), egui::pos2(rr.left() + 2.0, rr.bottom())),
                    0.0,
                    crate::theme::CYAN_DIM,
                );
                ui.add_space(2.0);
            }
            i = j;
        }

        if let Some(id) = to_delete {
            self.qso_log.retain(|q| q.id != id);
            persist_qso_log(&self.qso_log);
        } else if let Some(id) = to_edit {
            if let Some(r) = self.qso_log.iter().find(|q| q.id == id) {
                self.log_edit = Some(LogEditForm::from_record(r));
            }
        } else if let Some(id) = to_upload {
            if let Some(r) = self.qso_log.iter().find(|q| q.id == id) {
                let adif = sdroxide_types::qso_log_to_adif(std::slice::from_ref(r));
                self.pending_uploads.push((id, adif, up_targets));
            }
        }
    }

    fn settings_window(&mut self, ctx: &egui::Context, cmds: &mut Vec<Command>) {
        // Query slow lists (cpal devices, serial ports, radio config) once per
        // dialog-open; a pick invalidates so the selection refreshes.
        if !self.show_settings {
            self.audio_devices = None;
            self.audio_devices_queried = false;
            return;
        } else if !self.audio_devices_queried {
            self.audio_devices = self.ctrl.audio_devices();
            self.radio_cfg = self.ctrl.radio_config();
            self.serial_ports = self.ctrl.serial_ports();
            (self.midi_in_ports, self.midi_out_ports) = self.input.midi_ports();
            // The TCI server lives with the engine, so only a native client
            // owns its config; the browser remote gets `None` and a note.
            if let Some(cfg) = self.ctrl.tci_server_config() {
                self.tci_srv_edit = cfg;
                self.tci_srv_seeded = true;
            }
            if let Some(cfg) = self.ctrl.rigctld_config() {
                self.rigctld_edit = cfg;
                self.rigctld_seeded = true;
            }
            if let Some(cfg) = self.ctrl.wsjtx_config() {
                self.wsjtx_edit = cfg;
                self.wsjtx_seeded = true;
            }
            self.audio_devices_queried = true;
        }
        // Edits collected here and applied after the window closure, which
        // borrows `&self` and so can't touch `&mut self.ctrl`.
        let mut audio_pick: Option<(bool, Option<String>)> = None;
        let mut hpsdr_discover = false;
        let mut tci_test = false;
        let mut flex_discover = false;
        let mut flex_test = false;
        let mut apply_iface = false;
        let mut radio_edit = self.radio_cfg.clone();
        let mut ui_edit = self.ui_settings;
        let mut digi_edit = self.digi_cfg_edit.clone();
        let digi_seeded = self.digi_cfg_seeded;
        let mut net_edit = self.net_cfg_edit.clone();
        let mut net_cmds = self.net_cluster_cmds.clone();
        let mut net_apply = false;
        let mut net_sync = false;
        let mut tci_srv_edit = self.tci_srv_edit.clone();
        let mut tci_srv_apply = false;
        let mut rigctld_edit = self.rigctld_edit.clone();
        let mut rigctld_apply = false;
        let mut wsjtx_edit = self.wsjtx_edit.clone();
        let mut wsjtx_apply = false;
        let mut input_edit = self.input.cfg.clone();
        let mut key_capture = self.input.key_capture;
        let mut midi_learn = self.input.midi_learn;
        let mut midi_rescan = false;

        // The concrete interface types the user chooses between. SoapySDR only
        // appears when compiled in; there is no auto-detect (an unavailable
        // interface falls back to a null source so the user can reconfigure).
        let mut iface_opts: Vec<sdroxide_types::Backend> = Vec::new();
        if self.soapy_supported {
            iface_opts.push(sdroxide_types::Backend::Soapy);
        }
        iface_opts.push(sdroxide_types::Backend::Hpsdr);
        iface_opts.push(sdroxide_types::Backend::Cat);
        iface_opts.push(sdroxide_types::Backend::Tci);
        iface_opts.push(sdroxide_types::Backend::Flex);
        iface_opts.push(sdroxide_types::Backend::Icom);

        let mut tab = self.settings_tab;
        let mut open = self.show_settings;
        // The window does its own scrolling, so its bar can only be themed
        // through the context style — lend the palette for the length of the
        // call and hand the body back the normal one.
        let bars = crate::theme::ScrollPalette::push(ctx);
        let resp = egui::Window::new("Settings")
            .open(&mut open)
            .frame(crate::chrome::window_frame())
            .resizable(false)
            .vscroll(true)
            .show(ctx, |ui| {
                bars.restore(ui);
                self.settings_body(
                    ui,
                    cmds,
                    &mut SettingsIo {
                        iface_opts: &iface_opts,
                        radio_edit: &mut radio_edit,
                        audio_pick: &mut audio_pick,
                        hpsdr_discover: &mut hpsdr_discover,
                        tci_test: &mut tci_test,
                        flex_discover: &mut flex_discover,
                        flex_test: &mut flex_test,
                        apply_iface: &mut apply_iface,
                        ui_edit: &mut ui_edit,
                        digi_edit: &mut digi_edit,
                        digi_seeded,
                        net_edit: &mut net_edit,
                        net_cmds: &mut net_cmds,
                        net_apply: &mut net_apply,
                        net_sync: &mut net_sync,
                        tci_srv_edit: &mut tci_srv_edit,
                        tci_srv_apply: &mut tci_srv_apply,
                        rigctld_edit: &mut rigctld_edit,
                        rigctld_apply: &mut rigctld_apply,
                        wsjtx_edit: &mut wsjtx_edit,
                        wsjtx_apply: &mut wsjtx_apply,
                        input_edit: &mut input_edit,
                        key_capture: &mut key_capture,
                        midi_learn: &mut midi_learn,
                        midi_rescan: &mut midi_rescan,
                        tab: &mut tab,
                    },
                );
            });
        bars.pop(ctx);
        if let Some(r) = &resp {
            crate::chrome::paint_window_border(ctx, &r.response);
        }
        self.show_settings = open;
        self.settings_tab = tab;
        // Persist net-config edits (kept across frames) and apply on demand.
        self.net_cfg_edit = net_edit;
        self.net_cluster_cmds = net_cmds;
        if net_apply {
            self.net_cfg_edit.cluster.commands = self
                .net_cluster_cmds
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect();
            // The engine persists net.json when it applies this.
            cmds.push(Command::SetNetworkConfig(self.net_cfg_edit.clone()));
        }
        if net_sync {
            cmds.push(Command::SyncConfirmations);
        }
        self.input.key_capture = key_capture;
        self.input.midi_learn = midi_learn;
        if midi_rescan {
            (self.midi_in_ports, self.midi_out_ports) = self.input.midi_ports();
        }
        if input_edit != self.input.cfg {
            // Bindings take effect on the next frame and are written straight
            // out — a rebind the operator can't see saved is a rebind they
            // will make again after the next restart.
            self.input.cfg = input_edit;
            self.input.persist();
        }
        self.tci_srv_edit = tci_srv_edit;
        if tci_srv_apply {
            // The engine persists tciserver.json when it binds (or fails to).
            cmds.push(Command::SetTciServerConfig(self.tci_srv_edit.clone()));
        }
        self.rigctld_edit = rigctld_edit;
        if rigctld_apply {
            // The engine persists rigctld.json when it binds (or fails to).
            cmds.push(Command::SetRigctldConfig(self.rigctld_edit.clone()));
        }
        self.wsjtx_edit = wsjtx_edit;
        if wsjtx_apply {
            // The engine persists wsjtx.json when it opens the socket.
            cmds.push(Command::SetWsjtxConfig(self.wsjtx_edit.clone()));
        }
        if let Some((output, name)) = audio_pick {
            self.ctrl.set_audio_device(output, name);
            self.audio_devices_queried = false;
        }
        if hpsdr_discover {
            // Blocking LAN scan (~1.5 s); done after the window closure so it can
            // take `&self.ctrl`. Results feed the device dropdown next frame.
            self.hpsdr_devices = self.ctrl.discover_hpsdr();
        }
        if flex_discover {
            // Passive listen for radio announcements (~2.5 s); after the
            // closure so it can take `&self.ctrl`.
            self.flex_devices = self.ctrl.discover_flex();
        }
        if flex_test {
            if let Some(cfg) = &radio_edit {
                let ip = cfg.flex.target_ip().unwrap_or_default().to_string();
                self.flex_test_result = Some(if ip.trim().is_empty() {
                    Err("no radio selected — press Discover or enter an IP".into())
                } else {
                    self.ctrl.test_flex(&ip)
                });
            }
        }
        if tci_test {
            // Blocking connect (~up to 3 s); after the closure so it can take
            // `&self.ctrl`. The result is shown in the TCI section next frame.
            if let Some(cfg) = &radio_edit {
                self.tci_test_result = Some(self.ctrl.test_tci(&cfg.tci.address));
            }
        }
        if apply_iface {
            // Persist the latest edits, then rebuild the live source (no restart).
            if let Some(cfg) = &radio_edit {
                self.ctrl.set_radio_config(cfg.clone());
            }
            self.ctrl.reopen_source();
        }
        if radio_edit != self.radio_cfg {
            if let Some(cfg) = &radio_edit {
                self.ctrl.set_radio_config(cfg.clone());
            }
            self.radio_cfg = radio_edit;
        }
        if ui_edit != self.ui_settings {
            // Live: fps + averaging flow to the engine via the spectrum-config
            // diff next frame; waterfall speed is read each frame. Persist too.
            self.ui_settings = ui_edit;
            persist_ui_settings(&self.ui_settings);
        }
        // Callsign/grid from the General tab — same store as the FT8/SSTV setup
        // dialog. Only apply once seeded so we can't overwrite the engine's saved
        // config with defaults.
        if digi_seeded && digi_edit != self.digi_cfg_edit {
            self.digi_cfg_edit = digi_edit;
            cmds.push(Command::SetDigiConfig(self.digi_cfg_edit.clone()));
        }
    }

    /// The Settings body: a General tab (station identity + the sound devices)
    /// and a Radio tab whose single interface selector drives the
    /// interface-specific section below it.
    ///
    /// Everything the dialog changes goes out through `io`, because the window
    /// closure borrows `&self` — see [`SettingsIo`].
    fn settings_body(&self, ui: &mut egui::Ui, cmds: &mut Vec<Command>, io: &mut SettingsIo) {
        use sdroxide_types::Backend;

        // Wrapped: the tab strip no longer fits the window's width on one line.
        ui.horizontal_wrapped(|ui| {
            for (t, label) in [
                (SettingsTab::General, "General"),
                (SettingsTab::Radio, "Radio"),
                (SettingsTab::Ui, "UI"),
                (SettingsTab::Controls, "Controls"),
                (SettingsTab::Spots, "Spots"),
                (SettingsTab::FreeDv, "FreeDV"),
                (SettingsTab::Uploads, "Uploads"),
                (SettingsTab::Servers, "Servers"),
            ] {
                if crate::chrome::chip(ui, *io.tab == t, label).clicked() {
                    *io.tab = t;
                }
            }
        });
        ui.separator();

        let backend = io.radio_edit.as_ref().map(|c| c.backend);

        match io.tab {
            SettingsTab::General => {
                ui.label(RichText::new("Station").size(14.0).strong().color(crate::theme::CYAN));
                ui.add_space(6.0);
                if !io.digi_seeded {
                    ui.label(
                        RichText::new(
                            "Enter a digital mode (FT8 / SSTV / …) once to load the saved values.",
                        )
                        .weak(),
                    );
                }
                ui.add_enabled_ui(io.digi_seeded, |ui| {
                    egui::Grid::new("general-grid").num_columns(2).spacing([12.0, 8.0]).show(
                        ui,
                        |ui| {
                            ui.label("Callsign");
                            if ui.text_edit_singleline(&mut io.digi_edit.my_call).changed() {
                                io.digi_edit.my_call = io.digi_edit.my_call.to_uppercase();
                            }
                            ui.end_row();
                            ui.label("Grid square");
                            ui.text_edit_singleline(&mut io.digi_edit.my_grid);
                            ui.end_row();
                        },
                    );
                });
                ui.add_space(8.0);
                ui.label(
                    RichText::new(
                        "Your callsign and grid, shared across FT8/FT4, SSTV image headers, and \
                         the logbook. Also editable from the FT8 / SSTV setup dialog.",
                    )
                    .weak(),
                );

                ui.add_space(10.0);
                ui.separator();
                ui.add_space(6.0);
                self.settings_user_audio(ui, io.audio_pick);
                // The radio's own sound card is only used by the CAT / Audio
                // interface; every other backend carries its audio in-band.
                if backend == Some(Backend::Cat) {
                    if let (Some(devs), Some(cfg)) =
                        (self.audio_devices.as_ref(), io.radio_edit.as_mut())
                    {
                        ui.add_space(8.0);
                        ui.label(RichText::new("Radio audio (sound card)").strong());
                        egui::Grid::new("radio-audio").num_columns(2).spacing([12.0, 6.0]).show(
                            ui,
                            |ui| {
                                let (ci, co) =
                                    (cfg.radio_audio_in.clone(), cfg.radio_audio_out.clone());
                                ui.label("From radio (RX)");
                                device_combo(ui, "r-in", &devs.inputs, &ci, |n| {
                                    cfg.radio_audio_in = n
                                });
                                ui.end_row();
                                ui.label("To radio (TX)");
                                device_combo(ui, "r-out", &devs.outputs, &co, |n| {
                                    cfg.radio_audio_out = n
                                });
                                ui.end_row();
                            },
                        );
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            if ui
                                .button("Apply / reconnect")
                                .on_hover_text(
                                    "Reopen the CAT rig with these sound cards — no restart",
                                )
                                .clicked()
                            {
                                *io.apply_iface = true;
                            }
                            ui.label(
                                RichText::new("Reconnects the radio without restarting.").weak(),
                            );
                        });
                    }
                }
            }
            SettingsTab::Radio => {
                let Some(cfg) = io.radio_edit.as_mut() else {
                    ui.label("Radio configuration is only available in the native app.");
                    return;
                };
                // The single "which radio interface" selector.
                egui::Grid::new("iface-grid").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                    ui.label(RichText::new("Radio interface").strong());
                    enum_combo(ui, "iface", &mut cfg.backend, io.iface_opts, Backend::label);
                    ui.end_row();
                });
                ui.separator();

                match cfg.backend {
                    Backend::Soapy => {
                        self.settings_device_tab(ui, cmds);
                        ui.add_space(4.0);
                        ui.label(
                            RichText::new(
                                "Choose the SoapySDR device with --device or device_args in \
                                 config.toml.",
                            )
                            .weak(),
                        );
                    }
                    Backend::Hpsdr => {
                        settings_hpsdr_tab(
                            ui,
                            &self.hpsdr_devices,
                            io.radio_edit,
                            io.hpsdr_discover,
                        )
                    }
                    Backend::Cat => settings_cat_tab(ui, &self.serial_ports, io.radio_edit),
                    Backend::Tci => {
                        settings_tci_tab(ui, io.radio_edit, io.tci_test, &self.tci_test_result)
                    }
                    Backend::Icom => settings_icom_tab(ui, io.radio_edit),
                    Backend::Flex => settings_flex_tab(
                        ui,
                        &self.flex_devices,
                        io.radio_edit,
                        io.flex_discover,
                        io.flex_test,
                        &self.flex_test_result,
                    ),
                    // Legacy configs may still carry the removed auto-detect
                    // backend; prompt the user to pick a concrete interface.
                    Backend::Auto => {
                        ui.label(
                            RichText::new(
                                "Pick a radio interface above (this configuration used the \
                                 removed auto-detect mode).",
                            )
                            .weak(),
                        );
                    }
                }
                ui.separator();
                ui.horizontal(|ui| {
                    if ui
                        .button("Apply / reconnect")
                        .on_hover_text("Switch to this interface now — no restart needed")
                        .clicked()
                    {
                        *io.apply_iface = true;
                    }
                    ui.label(
                        RichText::new("Switches the live radio without restarting.").weak(),
                    );
                });
            }
            SettingsTab::Ui => settings_ui_tab(ui, io.ui_edit),
            SettingsTab::Spots => {
                operator_identity_note(ui, io.digi_edit, io.digi_seeded);

                net_heading(ui, "DX cluster (telnet)");
                ui.checkbox(&mut io.net_edit.cluster.enabled, "Enabled");
                net_row(ui, "Host", &mut io.net_edit.cluster.host, 220.0);
                ui.horizontal(|ui| {
                    ui.add_sized([96.0, 22.0], egui::Label::new("Port"));
                    ui.add(egui::DragValue::new(&mut io.net_edit.cluster.port).range(1..=65535));
                });
                net_row(ui, "Login call", &mut io.net_edit.cluster.login, 140.0);
                ui.horizontal(|ui| {
                    ui.add_sized([96.0, 22.0], egui::Label::new("Commands"));
                    ui.add(
                        egui::TextEdit::multiline(io.net_cmds)
                            .desired_rows(2)
                            .hint_text("one per line, e.g. SET/FT8")
                            .desired_width(220.0),
                    );
                });

                net_heading(ui, "POTA / SOTA / PSK Reporter");
                ui.checkbox(&mut io.net_edit.pota.enabled, "POTA activator spots");
                ui.checkbox(&mut io.net_edit.sota.enabled, "SOTA spots");
                ui.checkbox(&mut io.net_edit.psk.enabled, "PSK Reporter (current band)");
                ui.checkbox(&mut io.net_edit.psk.report, "Upload my FT8/FT4 decodes")
                    .on_hover_text(
                        "Report what this station hears to pskreporter.info, so it appears \
                         there as a receiver. Uses the callsign and grid from the General tab.",
                    );
                if io.net_edit.psk.report {
                    net_row(ui, "Antenna", &mut io.net_edit.psk.antenna, 200.0);
                    ui.horizontal(|ui| {
                        ui.add_sized([96.0, 22.0], egui::Label::new("Collector"));
                        ui.add(
                            egui::TextEdit::singleline(&mut io.net_edit.psk.host)
                                .desired_width(140.0),
                        );
                        ui.add(egui::DragValue::new(&mut io.net_edit.psk.port).range(1..=65535))
                            .on_hover_text("4739 is the live collector, 14739 the test one");
                    });
                }
                ui.horizontal(|ui| {
                    ui.add_sized([96.0, 22.0], egui::Label::new("Max age (s)"));
                    let age = &mut io.net_edit.spot_max_age_secs;
                    ui.add(egui::DragValue::new(age).range(60..=7200));
                });

                ui.add_space(8.0);
                if crate::chrome::chip_accent(
                    ui,
                    false,
                    RichText::new(" APPLY ").strong(),
                    crate::theme::GREEN,
                    crate::theme::INK_ON_CYAN,
                )
                .on_hover_text("Persist and (re)connect the feeds")
                .clicked()
                {
                    *io.net_apply = true;
                }
            }
            SettingsTab::Uploads => {
                net_heading(ui, "Callsign lookup");
                ui.horizontal(|ui| {
                    ui.add_sized([96.0, 22.0], egui::Label::new("Provider"));
                    egui::ComboBox::from_id_salt("lookup_provider")
                        .selected_text(io.net_edit.lookup_provider.label())
                        .show_ui(ui, |ui| {
                            for p in LookupProvider::ALL {
                                let cur = &mut io.net_edit.lookup_provider;
                                ui.selectable_value(cur, p, p.label());
                            }
                        });
                });
                ui.checkbox(
                    &mut io.net_edit.auto_lookup,
                    "Auto-fill name/QTH/grid on spot click & QSO",
                );
                net_row(ui, "QRZ user", &mut io.net_edit.qrz.user, 140.0);
                net_secret(ui, "QRZ pass", &mut io.net_edit.qrz.password, 140.0);
                net_row(ui, "HamQTH user", &mut io.net_edit.hamqth.user, 140.0);
                net_secret(ui, "HamQTH pass", &mut io.net_edit.hamqth.password, 140.0);

                net_heading(ui, "Upload — eQSL / QRZ / Club Log");
                net_row(ui, "eQSL user", &mut io.net_edit.eqsl.user, 140.0);
                net_secret(ui, "eQSL pass", &mut io.net_edit.eqsl.password, 140.0);
                net_secret(ui, "QRZ log key", &mut io.net_edit.qrz_logbook_key, 200.0);
                net_row(ui, "Club Log email", &mut io.net_edit.clublog.user, 200.0);
                net_secret(ui, "Club Log pass", &mut io.net_edit.clublog.password, 140.0);
                net_secret(ui, "Club Log key", &mut io.net_edit.clublog_api_key, 200.0);
                ui.checkbox(&mut io.net_edit.auto_upload, "Auto-upload each new QSO");
                ui.horizontal(|ui| {
                    ui.checkbox(&mut io.net_edit.auto_upload_eqsl, "eQSL");
                    ui.checkbox(&mut io.net_edit.auto_upload_qrz, "QRZ");
                    ui.checkbox(&mut io.net_edit.auto_upload_clublog, "Club Log");
                });

                net_heading(ui, "Confirmations (download)");
                net_row(ui, "LoTW user", &mut io.net_edit.lotw.user, 140.0);
                net_secret(ui, "LoTW pass", &mut io.net_edit.lotw.password, 140.0);
                ui.label(
                    RichText::new(
                        "LoTW upload uses TQSL — export ADIF from the logbook and sign it. \
                         LoTW/eQSL confirmations are downloaded here to mark worked-vs-confirmed.",
                    )
                    .size(10.5)
                    .color(Color32::from_gray(140)),
                );

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if crate::chrome::chip_accent(
                        ui,
                        false,
                        RichText::new(" APPLY ").strong(),
                        crate::theme::GREEN,
                        crate::theme::INK_ON_CYAN,
                    )
                    .clicked()
                    {
                        *io.net_apply = true;
                    }
                    if crate::chrome::chip(ui, false, "SYNC CONFIRMATIONS").clicked() {
                        *io.net_sync = true;
                    }
                });
            }
            SettingsTab::FreeDv => {
                // The reported identity is the operator's, from the General tab.
                let call = io.digi_edit.my_call.trim().to_string();
                let grid = io.digi_edit.my_grid.trim().to_string();
                settings_freedv_tab(
                    ui,
                    io.net_edit,
                    &call,
                    &grid,
                    io.digi_seeded,
                    &self.net_status,
                    io.net_apply,
                )
            }
            SettingsTab::Controls => settings_controls_tab(
                ui,
                io,
                &self.memories,
                &self.midi_in_ports,
                &self.midi_out_ports,
                &self.input.midi_status(),
                self.input.last_midi,
            ),
            SettingsTab::Servers => {
                settings_rigctld_tab(
                    ui,
                    io.rigctld_edit,
                    self.rigctld_seeded,
                    &self.rigctld_status,
                    io.rigctld_apply,
                );
                ui.add_space(12.0);
                ui.separator();
                ui.add_space(8.0);
                settings_tci_server_tab(
                    ui,
                    io.tci_srv_edit,
                    self.tci_srv_seeded,
                    &self.tci_srv_status,
                    io.tci_srv_apply,
                );
                ui.add_space(12.0);
                ui.separator();
                ui.add_space(8.0);
                settings_wsjtx_tab(ui, io.wsjtx_edit, self.wsjtx_seeded, io.wsjtx_apply);
            }
        }
    }

    /// The user's own speakers / microphone (applied live).
    fn settings_user_audio(
        &self,
        ui: &mut egui::Ui,
        audio_pick: &mut Option<(bool, Option<String>)>,
    ) {
        let Some(devs) = &self.audio_devices else {
            return;
        };
        ui.label(RichText::new("Your audio (speakers / microphone)").strong());
        egui::Grid::new("user-audio").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label("Output");
            device_combo(ui, "u-out", &devs.outputs, &devs.selected_output, |n| {
                *audio_pick = Some((true, n))
            });
            ui.end_row();
            ui.label("Input");
            device_combo(ui, "u-in", &devs.inputs, &devs.selected_input, |n| {
                *audio_pick = Some((false, n))
            });
            ui.end_row();
        });
    }

    /// SoapySDR RX/TX gains + antenna (empty for a CAT rig).
    fn settings_device_tab(&self, ui: &mut egui::Ui, cmds: &mut Vec<Command>) {
        let Some(caps) = &self.caps else {
            ui.label("no device");
            return;
        };
        ui.label(RichText::new(&caps.label).size(14.0).strong().color(crate::theme::CYAN));
        ui.add_space(6.0);
        if caps.gains.iter().all(|g| g.direction != Direction::Rx) {
            ui.label(RichText::new("This rig has no software-adjustable gains.").weak());
        }
        ui.label(RichText::new("RX gains").strong());
        egui::Grid::new("gains").num_columns(2).show(ui, |ui| {
            for g in caps.gains.iter().filter(|g| g.direction == Direction::Rx) {
                ui.label(&g.name);
                let mut db = self
                    .state
                    .gains
                    .iter()
                    .find(|(n, _)| *n == g.name)
                    .map(|(_, d)| *d)
                    .unwrap_or(g.min_db);
                let step = if g.step_db > 0.0 { g.step_db } else { 1.0 };
                if crate::chrome::slider(
                    ui,
                    Slider::new(&mut db, g.min_db..=g.max_db).step_by(step).suffix(" dB"),
                )
                .changed()
                {
                    cmds.push(Command::SetGain { dir: Direction::Rx, element: g.name.clone(), db });
                }
                ui.end_row();
            }
        });
        if caps.gains.iter().any(|g| g.direction == Direction::Tx) {
            ui.separator();
            ui.label(RichText::new("TX gains").strong().color(Color32::from_rgb(240, 90, 60)));
            egui::Grid::new("tx-gains").num_columns(2).show(ui, |ui| {
                for g in caps.gains.iter().filter(|g| g.direction == Direction::Tx) {
                    ui.label(&g.name);
                    let mut db = self
                        .state
                        .tx_gains
                        .iter()
                        .find(|(n, _)| *n == g.name)
                        .map(|(_, d)| *d)
                        .unwrap_or(g.min_db);
                    let step = if g.step_db > 0.0 { g.step_db } else { 1.0 };
                    if crate::chrome::slider(
                        ui,
                        Slider::new(&mut db, g.min_db..=g.max_db).step_by(step).suffix(" dB"),
                    )
                    .changed()
                    {
                        cmds.push(Command::SetGain {
                            dir: Direction::Tx,
                            element: g.name.clone(),
                            db,
                        });
                    }
                    ui.end_row();
                }
            });
        }
        if caps.antennas_rx.len() > 1 {
            ui.separator();
            ComboBox::from_id_salt("ant-rx")
                .selected_text(self.state.antenna_rx.clone())
                .show_ui(ui, |ui| {
                    for a in &caps.antennas_rx {
                        if ui.selectable_label(self.state.antenna_rx == *a, a).clicked() {
                            cmds.push(Command::SetAntenna { dir: Direction::Rx, name: a.clone() });
                        }
                    }
                });
        }
    }

}

/// A device dropdown ("System default" + names); calls `pick(Some(name)|None)`.
fn device_combo(
    ui: &mut egui::Ui,
    id: &str,
    names: &[String],
    selected: &Option<String>,
    mut pick: impl FnMut(Option<String>),
) {
    let shown = selected.clone().unwrap_or_else(|| "System default".into());
    ComboBox::from_id_salt(id).width(300.0).selected_text(shown).show_ui(ui, |ui| {
        if ui.selectable_label(selected.is_none(), "System default").clicked() {
            pick(None);
        }
        for n in names {
            if ui.selectable_label(selected.as_deref() == Some(n), n).clicked() {
                pick(Some(n.clone()));
            }
        }
    });
}

/// A dropdown over an enum's `ALL`, using its `label()`.
/// UI / display preferences: frame rate, waterfall scroll speed, spectrum speed.
/// Section heading for the Spots / Uploads settings tabs.
fn net_heading(ui: &mut egui::Ui, text: &str) {
    ui.add_space(6.0);
    ui.label(RichText::new(text).size(12.0).strong().color(crate::theme::CYAN));
}

/// A labelled single-line text field for the network settings tabs.
fn net_row(ui: &mut egui::Ui, label: &str, val: &mut String, w: f32) {
    ui.horizontal(|ui| {
        ui.add_sized([96.0, 22.0], egui::Label::new(label));
        ui.add(egui::TextEdit::singleline(val).desired_width(w));
    });
}

/// A labelled password field (masked) for the network settings tabs.
fn net_secret(ui: &mut egui::Ui, label: &str, val: &mut String, w: f32) {
    ui.horizontal(|ui| {
        ui.add_sized([96.0, 22.0], egui::Label::new(label));
        ui.add(egui::TextEdit::singleline(val).password(true).desired_width(w));
    });
}

fn settings_ui_tab(ui: &mut egui::Ui, cfg: &mut sdroxide_types::UiSettings) {
    use sdroxide_types::{Speed, UiSettings};
    ui.label(RichText::new("Display").size(14.0).strong().color(crate::theme::CYAN));
    ui.add_space(6.0);
    egui::Grid::new("ui-grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
        ui.label("Screen update rate");
        ComboBox::from_id_salt("ui-fps")
            .selected_text(format!("{} fps", cfg.frame_rate_fps))
            .show_ui(ui, |ui| {
                for f in UiSettings::FPS_OPTIONS {
                    ui.selectable_value(&mut cfg.frame_rate_fps, f, format!("{f} fps"));
                }
            });
        ui.end_row();

        ui.label("Waterfall scroll speed");
        enum_combo(ui, "ui-wf", &mut cfg.waterfall_speed, &Speed::ALL, Speed::label);
        ui.end_row();

        ui.label("Spectrum update speed");
        enum_combo(ui, "ui-spec", &mut cfg.spectrum_speed, &Speed::ALL, Speed::label);
        ui.end_row();

        ui.label("Waterfall palette");
        ComboBox::from_id_salt("ui-palette")
            .selected_text(colormap::NAMES[cfg.waterfall_palette.min(colormap::NAMES.len() - 1)])
            .show_ui(ui, |ui| {
                for (i, name) in colormap::NAMES.iter().enumerate() {
                    ui.selectable_value(&mut cfg.waterfall_palette, i, *name);
                }
            });
        ui.end_row();

        ui.label("Spectrum background");
        ui.horizontal(|ui| {
            ui.checkbox(&mut cfg.spectrum_gradient, "Gradient");
            ui.add_enabled_ui(cfg.spectrum_gradient, |ui| {
                ui.label("top");
                ui.color_edit_button_srgb(&mut cfg.gradient_top);
                ui.label("bottom");
                ui.color_edit_button_srgb(&mut cfg.gradient_bottom);
            });
        });
        ui.end_row();
    });
    ui.add_space(8.0);
    ui.label(
        RichText::new(
            "Higher frame rates look smoother but cost more CPU/GPU. Spectrum speed \
             sets how quickly the trace reacts (slower = smoother/more averaged). The \
             background gradient fills the spectrum area from the top colour down to \
             the bottom colour.",
        )
        .weak(),
    );
}

fn enum_combo<T: PartialEq + Copy>(
    ui: &mut egui::Ui,
    id: &str,
    cur: &mut T,
    all: &[T],
    label: impl Fn(T) -> &'static str,
) {
    ComboBox::from_id_salt(id).selected_text(label(*cur)).show_ui(ui, |ui| {
        for &opt in all {
            if ui.selectable_label(*cur == opt, label(opt)).clicked() {
                *cur = opt;
            }
        }
    });
}

/// CAT / Audio interface: serial + PTT parameters (the interface itself is
/// chosen by the selector in `settings_body`).
fn settings_cat_tab(
    ui: &mut egui::Ui,
    serial_ports: &[String],
    radio_edit: &mut Option<sdroxide_types::RadioConfig>,
) {
    use sdroxide_types::{CatFamily, DigiMode, LineState, ModeControl, Parity, PttMethod, SoundFormat, StopBits};
    let Some(cfg) = radio_edit.as_mut() else {
        ui.label("Radio configuration is only available in the native app.");
        return;
    };
    egui::Grid::new("cat-grid").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
        ui.label("Sound format");
        enum_combo(ui, "sfmt", &mut cfg.cat.format, &SoundFormat::ALL, SoundFormat::label);
        ui.end_row();

        if matches!(cfg.cat.format, SoundFormat::DemodAudio) {
            ui.label("Panadapter BW");
            ui.add(DragValue::new(&mut cfg.cat.audio_bw_hz).speed(100.0).range(1000.0..=24000.0).suffix(" Hz"));
            ui.end_row();
        }

        ui.label("Serial port");
        let shown = if cfg.cat.serial.path.is_empty() { "— select —".to_string() } else { cfg.cat.serial.path.clone() };
        ComboBox::from_id_salt("serport").width(260.0).selected_text(shown).show_ui(ui, |ui| {
            for p in serial_ports {
                if ui.selectable_label(&cfg.cat.serial.path == p, p).clicked() {
                    cfg.cat.serial.path = p.clone();
                }
            }
        });
        ui.end_row();

        ui.label("CAT family");
        enum_combo(ui, "fam", &mut cfg.cat.family, &CatFamily::ALL, CatFamily::label);
        ui.end_row();

        ui.label("Baud");
        ComboBox::from_id_salt("baud").selected_text(cfg.cat.serial.baud.to_string()).show_ui(ui, |ui| {
            for b in [4800u32, 9600, 19200, 38400, 57600, 115200] {
                if ui.selectable_label(cfg.cat.serial.baud == b, b.to_string()).clicked() {
                    cfg.cat.serial.baud = b;
                }
            }
        });
        ui.end_row();

        ui.label("Data bits");
        ComboBox::from_id_salt("databits").selected_text(cfg.cat.serial.data_bits.to_string()).show_ui(ui, |ui| {
            for d in [7u8, 8] {
                if ui.selectable_label(cfg.cat.serial.data_bits == d, d.to_string()).clicked() {
                    cfg.cat.serial.data_bits = d;
                }
            }
        });
        ui.end_row();

        ui.label("Parity");
        enum_combo(ui, "parity", &mut cfg.cat.serial.parity, &Parity::ALL, Parity::label);
        ui.end_row();

        ui.label("Stop bits");
        enum_combo(ui, "stop", &mut cfg.cat.serial.stop_bits, &StopBits::ALL, StopBits::label);
        ui.end_row();

        ui.label("Force RTS");
        enum_combo(ui, "rts", &mut cfg.cat.serial.force_rts, &LineState::ALL, LineState::label);
        ui.end_row();
        ui.label("Force DTR");
        enum_combo(ui, "dtr", &mut cfg.cat.serial.force_dtr, &LineState::ALL, LineState::label);
        ui.end_row();

        ui.label("PTT method");
        enum_combo(ui, "ptt", &mut cfg.cat.ptt, &PttMethod::ALL, PttMethod::label);
        ui.end_row();

        ui.label("Mode control");
        enum_combo(ui, "modectl", &mut cfg.cat.mode_control, &ModeControl::ALL, ModeControl::label);
        ui.end_row();

        ui.label("Digimode mode");
        enum_combo(ui, "digimode", &mut cfg.cat.digi_mode, &DigiMode::ALL, DigiMode::label);
        ui.end_row();

        ui.label("Poll rate");
        ui.add(DragValue::new(&mut cfg.cat.poll_hz).speed(0.5).range(0.5..=20.0).suffix(" Hz"));
        ui.end_row();

        if matches!(cfg.cat.family, CatFamily::Icom | CatFamily::Xiegu) {
            ui.label("Radio ID (hex)");
            let mut hex = format!("{:02X}", cfg.cat.icom_radio_id);
            let resp = ui.add(egui::TextEdit::singleline(&mut hex).desired_width(48.0));
            if resp.changed() {
                if let Ok(v) = u8::from_str_radix(hex.trim().trim_start_matches("0x"), 16) {
                    cfg.cat.icom_radio_id = v;
                }
            }
            ui.end_row();
        }
    });
    ui.add_space(6.0);
    ui.label(RichText::new("Press \"Apply / reconnect\" to switch without a restart.").weak());
}

/// HPSDR interface: network device discovery / manual IP / sample rate (the
/// interface itself is chosen by the selector in `settings_body`).
fn settings_hpsdr_tab(
    ui: &mut egui::Ui,
    devices: &[sdroxide_types::HpsdrDevice],
    radio_edit: &mut Option<sdroxide_types::RadioConfig>,
    discover: &mut bool,
) {
    use sdroxide_types::HpsdrConfig;
    let Some(cfg) = radio_edit.as_mut() else {
        ui.label("Radio configuration is only available in the native app.");
        return;
    };
    egui::Grid::new("hpsdr-grid").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
        ui.label("Devices");
        ui.horizontal(|ui| {
            if ui.button("Discover").clicked() {
                *discover = true;
            }
            let shown = cfg.hpsdr.selected_ip.clone().unwrap_or_else(|| "— none —".into());
            ComboBox::from_id_salt("hpsdr_dev").width(320.0).selected_text(shown).show_ui(ui, |ui| {
                if devices.is_empty() {
                    ui.label(RichText::new("no devices — press Discover").weak());
                }
                for d in devices {
                    // Only Protocol 2 devices are selectable; P1 (e.g. HL2) is shown but greyed.
                    if d.supported() {
                        let sel = cfg.hpsdr.selected_ip.as_deref() == Some(d.ip.as_str());
                        if ui.selectable_label(sel, d.label()).clicked() {
                            cfg.hpsdr.selected_ip = Some(d.ip.clone());
                        }
                    } else {
                        ui.label(RichText::new(d.label()).weak());
                    }
                }
            });
        });
        ui.end_row();

        ui.label("Manual IP");
        let mut ip = cfg.hpsdr.manual_ip.clone().unwrap_or_default();
        let resp = ui.add(
            egui::TextEdit::singleline(&mut ip)
                .desired_width(160.0)
                .hint_text("optional, e.g. 192.168.1.50"),
        );
        if resp.changed() {
            let t = ip.trim();
            cfg.hpsdr.manual_ip = if t.is_empty() { None } else { Some(t.to_string()) };
        }
        ui.end_row();

        ui.label("Sample rate");
        // Show only rates valid for the selected device's protocol (P1 ≤ 384 kHz).
        let proto = devices
            .iter()
            .find(|d| Some(d.ip.as_str()) == cfg.hpsdr.selected_ip.as_deref())
            .map(|d| d.protocol)
            .unwrap_or(2);
        let shown = format!("{} kHz", (cfg.hpsdr.sample_rate_hz / 1000.0) as u32);
        ComboBox::from_id_salt("hpsdr_rate").selected_text(shown).show_ui(ui, |ui| {
            for &r in HpsdrConfig::rates_for(proto) {
                let sel = (cfg.hpsdr.sample_rate_hz - r).abs() < 1.0;
                if ui.selectable_label(sel, format!("{} kHz", (r / 1000.0) as u32)).clicked() {
                    cfg.hpsdr.sample_rate_hz = r;
                }
            }
        });
        ui.end_row();
    });
    ui.add_space(6.0);
    ui.label(
        RichText::new(
            "A manual IP overrides discovery. Press \"Apply / reconnect\" to switch without a restart.",
        )
        .weak(),
    );
}

/// TCI interface: WebSocket server address, IQ sample rate, and a
/// Test-connection button (the interface is chosen by the selector in
/// `settings_body`).
fn settings_tci_tab(
    ui: &mut egui::Ui,
    radio_edit: &mut Option<sdroxide_types::RadioConfig>,
    tci_test: &mut bool,
    test_result: &Option<Result<String, String>>,
) {
    use sdroxide_types::TciConfig;
    let Some(cfg) = radio_edit.as_mut() else {
        ui.label("Radio configuration is only available in the native app.");
        return;
    };
    egui::Grid::new("tci-grid").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
        ui.label("Server address");
        ui.add(
            egui::TextEdit::singleline(&mut cfg.tci.address)
                .desired_width(220.0)
                .hint_text("host:port, e.g. 127.0.0.1:50001"),
        );
        ui.end_row();

        ui.label("IQ sample rate");
        let shown = format!("{} kHz", (cfg.tci.iq_sample_rate_hz / 1000.0) as u32);
        ComboBox::from_id_salt("tci_rate").selected_text(shown).show_ui(ui, |ui| {
            for &r in &TciConfig::IQ_RATES {
                let sel = (cfg.tci.iq_sample_rate_hz - r).abs() < 1.0;
                if ui.selectable_label(sel, format!("{} kHz", (r / 1000.0) as u32)).clicked() {
                    cfg.tci.iq_sample_rate_hz = r;
                }
            }
        });
        ui.end_row();

        ui.label("");
        if ui.button("Test connection").clicked() {
            *tci_test = true;
        }
        ui.end_row();
    });
    match test_result {
        Some(Ok(s)) => {
            ui.label(RichText::new(format!("Connected: {s}")).color(Color32::from_rgb(90, 200, 110)));
        }
        Some(Err(e)) => {
            ui.label(RichText::new(format!("Failed: {e}")).color(Color32::from_rgb(230, 90, 80)));
        }
        None => {}
    }
    ui.add_space(6.0);
    ui.label(
        RichText::new(
            "Wideband IQ receive, audio transmit. Press \"Apply / reconnect\" to switch without a restart.",
        )
        .weak(),
    );
}

/// FlexRadio interface: radio discovery / manual IP, DAX IQ channel and rate,
/// antenna, and a Test-connection button (the interface itself is chosen by the
/// selector in `settings_body`).
fn settings_flex_tab(
    ui: &mut egui::Ui,
    devices: &[sdroxide_types::FlexDevice],
    radio_edit: &mut Option<sdroxide_types::RadioConfig>,
    discover: &mut bool,
    test: &mut bool,
    test_result: &Option<Result<String, String>>,
) {
    use sdroxide_types::FlexConfig;
    let Some(cfg) = radio_edit.as_mut() else {
        ui.label("Radio configuration is only available in the native app.");
        return;
    };
    egui::Grid::new("flex-grid").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
        ui.label("Radios");
        ui.horizontal(|ui| {
            if ui
                .button("Discover")
                .on_hover_text("Listen for radios announcing themselves (about 2 s)")
                .clicked()
            {
                *discover = true;
            }
            let shown = cfg.flex.selected_ip.clone().unwrap_or_else(|| "— none —".into());
            ComboBox::from_id_salt("flex_dev").width(320.0).selected_text(shown).show_ui(ui, |ui| {
                if devices.is_empty() {
                    ui.label(RichText::new("no radios — press Discover").weak());
                }
                for d in devices {
                    let sel = cfg.flex.selected_ip.as_deref() == Some(d.ip.as_str());
                    if ui.selectable_label(sel, d.label()).clicked() {
                        cfg.flex.selected_ip = Some(d.ip.clone());
                    }
                }
            });
        });
        ui.end_row();

        ui.label("Manual IP");
        let mut ip = cfg.flex.manual_ip.clone().unwrap_or_default();
        let resp = ui.add(
            egui::TextEdit::singleline(&mut ip)
                .desired_width(160.0)
                .hint_text("optional, e.g. 192.168.1.60"),
        );
        if resp.changed() {
            let t = ip.trim();
            cfg.flex.manual_ip = if t.is_empty() { None } else { Some(t.to_string()) };
        }
        ui.end_row();

        ui.label("DAX IQ rate");
        let shown = format!("{} kHz", (cfg.flex.iq_sample_rate_hz / 1000.0) as u32);
        ComboBox::from_id_salt("flex_rate").selected_text(shown).show_ui(ui, |ui| {
            for &r in &FlexConfig::IQ_RATES {
                let sel = (cfg.flex.iq_sample_rate_hz - r).abs() < 1.0;
                if ui.selectable_label(sel, format!("{} kHz", (r / 1000.0) as u32)).clicked() {
                    cfg.flex.iq_sample_rate_hz = r;
                }
            }
        });
        ui.end_row();

        ui.label("DAX IQ channel");
        ComboBox::from_id_salt("flex_ch")
            .selected_text(cfg.flex.daxiq_channel.to_string())
            .show_ui(ui, |ui| {
                for ch in FlexConfig::CHANNELS {
                    if ui.selectable_label(cfg.flex.daxiq_channel == ch, ch.to_string()).clicked() {
                        cfg.flex.daxiq_channel = ch;
                    }
                }
            });
        ui.end_row();

        ui.label("Antenna");
        ui.add(
            egui::TextEdit::singleline(&mut cfg.flex.antenna)
                .desired_width(120.0)
                .hint_text("optional, e.g. ANT1"),
        );
        ui.end_row();

        ui.label("Station name");
        ui.add(
            egui::TextEdit::singleline(&mut cfg.flex.station)
                .desired_width(160.0)
                .hint_text("shown in the radio's client list"),
        );
        ui.end_row();

        ui.label("");
        if ui.button("Test connection").clicked() {
            *test = true;
        }
        ui.end_row();
    });
    match test_result {
        Some(Ok(s)) => {
            ui.label(RichText::new(format!("Connected: {s}")).color(Color32::from_rgb(90, 200, 110)));
        }
        Some(Err(e)) => {
            ui.label(RichText::new(format!("Failed: {e}")).color(Color32::from_rgb(230, 90, 80)));
        }
        None => {}
    }
    ui.add_space(6.0);
    ui.label(
        RichText::new(
            "Wideband DAX IQ receive, DAX audio transmit. sdroxide connects as a GUI client and \
             creates its own panadapter and slice. A manual IP overrides discovery; press \
             \"Apply / reconnect\" to switch without a restart.",
        )
        .weak(),
    );
}

/// Icom network interface: the radio's address and the network-control login,
/// plus which model it is (that decides the CI-V address).
fn settings_icom_tab(ui: &mut egui::Ui, radio_edit: &mut Option<sdroxide_types::RadioConfig>) {
    use sdroxide_types::IcomConfig;
    let Some(cfg) = radio_edit.as_mut() else {
        ui.label("Radio configuration is only available in the native app.");
        return;
    };
    egui::Grid::new("icom-grid").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
        ui.label("Radio IP");
        ui.add(
            egui::TextEdit::singleline(&mut cfg.icom.ip)
                .desired_width(160.0)
                .hint_text("e.g. 192.168.1.40"),
        );
        ui.end_row();

        ui.label("Model");
        let shown = cfg.icom.model.clone();
        ComboBox::from_id_salt("icom_model").selected_text(shown).show_ui(ui, |ui| {
            for (name, addr) in IcomConfig::MODELS {
                if ui.selectable_label(cfg.icom.model == name, name).clicked() {
                    cfg.icom.model = name.to_string();
                    // The CI-V address goes with the model; an operator who has
                    // changed it on the radio can still override it below.
                    cfg.icom.civ_address = addr;
                }
            }
        });
        ui.end_row();

        ui.label("CI-V address");
        let mut hex = format!("{:02X}", cfg.icom.civ_address);
        if ui.add(egui::TextEdit::singleline(&mut hex).desired_width(48.0)).changed()
            && let Ok(v) = u8::from_str_radix(hex.trim().trim_start_matches("0x"), 16)
        {
            cfg.icom.civ_address = v;
        }
        ui.end_row();

        ui.label("Username");
        ui.add(
            egui::TextEdit::singleline(&mut cfg.icom.username)
                .desired_width(160.0)
                .hint_text("as set on the radio"),
        );
        ui.end_row();

        ui.label("Password");
        ui.add(
            egui::TextEdit::singleline(&mut cfg.icom.password).desired_width(160.0).password(true),
        );
        ui.end_row();

        ui.label("Scope span");
        let shown = format!("\u{b1}{} kHz", (cfg.icom.scope_span_hz / 1000.0) as u32);
        ComboBox::from_id_salt("icom_span").selected_text(shown).show_ui(ui, |ui| {
            for hz in [2_500.0, 5_000.0, 10_000.0, 25_000.0, 50_000.0, 100_000.0, 250_000.0, 500_000.0] {
                let sel = (cfg.icom.scope_span_hz - hz).abs() < 1.0;
                let label = if hz < 1000.0 {
                    format!("\u{b1}{hz} Hz")
                } else {
                    format!("\u{b1}{} kHz", (hz / 1000.0) as u32)
                };
                if ui.selectable_label(sel, label).clicked() {
                    cfg.icom.scope_span_hz = hz;
                }
            }
        });
        ui.end_row();

        ui.label("Audio-band width");
        let mut khz = cfg.icom.audio_bw_hz / 1000.0;
        if crate::chrome::slider(ui, Slider::new(&mut khz, 3.0..=12.0).suffix(" kHz")).changed() {
            cfg.icom.audio_bw_hz = khz * 1000.0;
        }
        ui.end_row();
    });
    ui.add_space(6.0);
    ui.label(
        RichText::new(
            "Control, audio and the radio's own spectrum scope all travel over the network — \
             no cable, no sound card and no wfview in between. Enable network control on the \
             radio (Set → Network) and set the same username and password there; only one \
             network client can be connected at a time. The waterfall is the radio's own scope, \
             so the SPAN button on the radio moves it too; the audio-band width below is what \
             the panadapter falls back to while no sweep is arriving. Press \"Apply / \
             reconnect\" to switch without a restart.",
        )
        .weak(),
    );
    ui.add_space(4.0);
    ui.label(
        RichText::new(
            "To transmit with the computer's microphone instead of the one plugged into the \
             radio, the radio has to be told to take its modulation from the network: \
             MENU → SET → Connectors → MOD Input → DATA OFF MOD = WLAN (or LAN) for voice, \
             and DATA MOD = WLAN for the digital modes. Without it the radio keys but \
             modulates from its own microphone jack, and nothing this end can change that.",
        )
        .weak(),
    );
}

/// Live status of the built-in TCI server, from `RadioEvent::TciServerStatus`.
#[derive(Clone, PartialEq)]
struct TciServerStatus {
    running: bool,
    addr: String,
    clients: usize,
    error: Option<String>,
}

/// Where the operator's callsign and grid actually live, for the network tabs
/// that report under them but deliberately do not offer a second copy to edit.
fn operator_identity_note(
    ui: &mut egui::Ui,
    digi: &sdroxide_types::DigiConfig,
    seeded: bool,
) {
    net_heading(ui, "Operator");
    if !seeded {
        ui.label(RichText::new("Callsign and grid are set on the General tab.").weak());
        return;
    }
    let (call, grid) = (digi.my_call.trim(), digi.my_grid.trim());
    if call.is_empty() || grid.is_empty() {
        ui.label(
            RichText::new("⚠ Set your callsign and grid on the General tab.")
                .color(Color32::from_rgb(230, 170, 60)),
        );
    } else {
        ui.label(RichText::new(format!("{call} / {grid}  — set on the General tab")).weak());
    }
}

/// FreeDV Reporter (<https://qso.freedv.org/>): announce this station and show
/// everyone else's as spots.
///
/// `call`/`grid` are the operator identity from the General tab, shown here but
/// not editable: reporting under a second copy would only let the two disagree.
#[allow(clippy::too_many_arguments)]
fn settings_freedv_tab(
    ui: &mut egui::Ui,
    net: &mut sdroxide_types::NetworkConfig,
    call: &str,
    grid: &str,
    digi_seeded: bool,
    status: &Option<String>,
    apply: &mut bool,
) {
    ui.label(RichText::new("FreeDV Reporter").size(14.0).strong().color(crate::theme::CYAN));
    ui.add_space(6.0);
    ui.checkbox(&mut net.freedv_reporter.enabled, "Enable").on_hover_text(
        "Connects whenever enabled. Your station is only shown to others while the radio is \
         in RADE — in any other mode you stay connected but hidden.",
    );
    ui.add_space(6.0);

    let enabled = net.freedv_reporter.enabled;

    ui.add_enabled_ui(enabled, |ui| {
        let c = &mut net.freedv_reporter;

        net_heading(ui, "Station");
        net_row(ui, "Message", &mut c.message, 260.0);
        ui.checkbox(&mut c.rx_only, "Receive only (I cannot transmit)");

        net_heading(ui, "Server");
        net_row(ui, "Host", &mut c.host, 220.0);
        ui.horizontal(|ui| {
            ui.add_sized([96.0, 22.0], egui::Label::new("Port"));
            ui.add(egui::DragValue::new(&mut c.port).range(1..=65535));
        });
        ui.add_enabled_ui(false, |ui| {
            ui.checkbox(&mut c.tls, "TLS (wss://)")
                .on_hover_text("Not yet implemented — FreeDV GUI uses plain ws:// too.");
        });

        net_heading(ui, "Reporting");
        ui.checkbox(&mut c.report_rx, "Report stations I decode")
            .on_hover_text("Sends an rx_report for each callsign recovered from a RADE \
                            End-of-Over frame.");
        ui.checkbox(&mut c.show_spots, "Show other reporter stations as spots")
            .on_hover_text("Adds them to the panadapter overlay, world map and SPOTS window \
                            under the FREEDV filter.");
    });

    ui.add_space(8.0);
    if enabled && digi_seeded && (call.is_empty() || grid.is_empty()) {
        ui.label(
            RichText::new(
                "⚠ Set your callsign and grid on the General tab. Without both, the \
                 connection is view-only: you will see other stations but will not appear \
                 yourself.",
            )
            .color(Color32::from_rgb(230, 170, 60)),
        );
    } else if enabled && digi_seeded {
        ui.label(
            RichText::new(format!(
                "Reporting as {call} / {grid} — SDRoxide {}",
                env!("CARGO_PKG_VERSION")
            ))
            .weak(),
        );
        ui.label(RichText::new("Callsign and grid are set on the General tab.").weak());
    }
    // The status line is shared by every network feed, so only show it here
    // when it is actually ours.
    if let Some(s) = status {
        if s.starts_with("FreeDV Reporter") {
            ui.label(RichText::new(s).weak());
        }
    }

    ui.add_space(8.0);
    if crate::chrome::chip_accent(
        ui,
        false,
        RichText::new(" APPLY ").strong(),
        crate::theme::GREEN,
        crate::theme::INK_ON_CYAN,
    )
    .on_hover_text("Persist and (re)connect")
    .clicked()
    {
        *apply = true;
    }
}

/// The built-in TCI *server*: this app acting as a TCI rig for third-party
/// clients (WSJT-X's TCI rig type, JTDX, MSHV, skimmers). Distinct from the TCI
/// *client* section on the Radio tab, which connects sdroxide to another rig.

/// A dropdown over every bindable [`Action`], grouped by section. `memories`
/// contributes one recall entry per stored channel, since those are the only
/// actions whose parameter comes from the operator's own data.
fn action_combo(
    ui: &mut egui::Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    action: &mut sdroxide_types::Action,
    memories: &[MemoryChannel],
) -> bool {
    use sdroxide_types::Action;
    let mut changed = false;
    ComboBox::from_id_salt(id).width(210.0).selected_text(action.label()).show_ui(ui, |ui| {
        let mut group = "";
        let all = Action::all()
            .into_iter()
            .chain(memories.iter().map(|m| Action::MemoryRecall(m.id)));
        for a in all {
            if a.group() != group {
                group = a.group();
                ui.add_space(4.0);
                ui.label(RichText::new(group).small().weak());
            }
            if ui.selectable_label(*action == a, a.label()).clicked() {
                *action = a;
                changed = true;
            }
        }
    });
    changed
}

/// Keyboard chords, panadapter mouse behaviour and mouse-button bindings.
///
/// Edits here are live and self-persisting; there is no APPLY chip, because a
/// binding is only useful once it is already in effect.
#[allow(clippy::too_many_arguments)]
fn settings_controls_tab(
    ui: &mut egui::Ui,
    io: &mut SettingsIo,
    memories: &[MemoryChannel],
    midi_in: &[(String, String)],
    midi_out: &[(String, String)],
    midi_status: &crate::input::MidiStatusView,
    last_midi: Option<(sdroxide_types::MidiMsg, u8)>,
) {
    use sdroxide_types::{
        Action, ActionKind, ButtonMode, KeyBinding, MouseButton, MouseButtonBinding, WheelAction,
        WheelSettings,
    };
    let cfg = &mut *io.input_edit;
    let key_capture = &mut *io.key_capture;

    ui.label(RichText::new("Keyboard").size(14.0).strong().color(crate::theme::CYAN));
    ui.add_space(4.0);
    ui.label(
        RichText::new(
            "Click a shortcut to rebind it, then press the key combination (Esc cancels). \
             Bindings are ignored while you are typing in a text field.",
        )
        .weak(),
    );
    ui.add_space(6.0);

    let mut remove: Option<usize> = None;
    egui::Grid::new("keys-grid").num_columns(6).spacing([10.0, 6.0]).striped(true).show(
        ui,
        |ui| {
            ui.label(RichText::new("Shortcut").small().weak());
            ui.label(RichText::new("Does").small().weak());
            ui.label(RichText::new("Step / mode").small().weak());
            ui.label(RichText::new("Accel").small().weak());
            ui.label(RichText::new("On").small().weak());
            ui.label("");
            ui.end_row();

            for (i, b) in cfg.keys.iter_mut().enumerate() {
                let capturing = *key_capture == Some(i);
                let label = if capturing { "press a key…".to_string() } else { b.chord.label() };
                if crate::chrome::chip(ui, capturing, RichText::new(label).monospace()).clicked() {
                    *key_capture = if capturing { None } else { Some(i) };
                }

                if action_combo(ui, ("keyact", i), &mut b.action, memories) {
                    b.tuning.step = b.action.default_step();
                }

                match b.action.kind() {
                    ActionKind::Continuous => {
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::DragValue::new(&mut b.tuning.step)
                                    .speed(1.0)
                                    .range(0.0001..=1_000_000.0),
                            );
                            // The sign of `value` is the direction, so one
                            // action can have an up key and a down key.
                            let mut down = b.value < 0.0;
                            if ui.checkbox(&mut down, "down").changed() {
                                b.value = if down { -1.0 } else { 1.0 };
                            }
                        });
                        ui.add(egui::DragValue::new(&mut b.tuning.accel).speed(0.05).range(0.0..=4.0));
                    }
                    ActionKind::Momentary => {
                        ui.horizontal(|ui| {
                            for m in ButtonMode::ALL {
                                if crate::chrome::chip(ui, b.button == m, m.label()).clicked() {
                                    b.button = m;
                                }
                            }
                        });
                        ui.label("");
                    }
                }

                ui.checkbox(&mut b.enabled, "");
                if ui.small_button("✕").on_hover_text("Remove this binding").clicked() {
                    remove = Some(i);
                }
                ui.end_row();
            }
        },
    );
    if let Some(i) = remove {
        cfg.keys.remove(i);
        if *key_capture == Some(i) {
            *key_capture = None;
        }
    }

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if crate::chrome::chip(ui, false, "+ Add shortcut").clicked() {
            cfg.keys.push(KeyBinding::default());
            *key_capture = Some(cfg.keys.len() - 1);
        }
        if crate::chrome::chip(ui, false, "Restore defaults").clicked() {
            cfg.keys = KeyBinding::defaults();
            *key_capture = None;
        }
        // PTT ships unbound on purpose; this is the one-click opt-in.
        let has_ptt = cfg.keys.iter().any(|b| b.action == Action::Ptt);
        if !has_ptt
            && crate::chrome::chip(ui, false, "Bind hold-to-talk to Space")
                .on_hover_text("Hold Space to transmit; releasing it — or losing window focus — unkeys")
                .clicked()
        {
            cfg.keys.push(KeyBinding {
                chord: sdroxide_types::KeyChord::plain("Space"),
                action: Action::Ptt,
                button: ButtonMode::Momentary,
                ..KeyBinding::default()
            });
        }
    });

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label("Unkey a held PTT after");
        ui.add(
            egui::DragValue::new(&mut cfg.ptt_hold_timeout_s)
                .speed(5.0)
                .range(0.0..=3600.0)
                .suffix(" s"),
        );
    })
    .response
    .on_hover_text("Backstop against a stuck key or a controller that stops reporting. 0 disables.");

    ui.add_space(10.0);
    ui.separator();
    ui.add_space(6.0);
    ui.label(RichText::new("Panadapter mouse").size(14.0).strong().color(crate::theme::CYAN));
    ui.add_space(6.0);

    let w = &mut cfg.wheel;
    egui::Grid::new("mouse-grid").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
        ui.label("Wheel");
        wheel_action_combo(ui, "wheel-plain", &mut w.wheel);
        ui.end_row();

        ui.label("Wheel + Shift");
        wheel_action_combo(ui, "wheel-shift", &mut w.wheel_shift);
        ui.end_row();

        ui.label("Tune step");
        ui.add(
            egui::DragValue::new(&mut w.tune_step_hz).speed(10.0).range(1.0..=1e6).suffix(" Hz"),
        );
        ui.end_row();

        ui.label("Zoom rate");
        ui.add(egui::DragValue::new(&mut w.zoom_rate).speed(0.05).range(0.1..=5.0));
        ui.end_row();

        ui.label("Click-tune rounding");
        ui.add(
            egui::DragValue::new(&mut w.click_tune_step_hz)
                .speed(1.0)
                .range(1.0..=10_000.0)
                .suffix(" Hz"),
        );
        ui.end_row();
    });
    ui.add_space(4.0);
    ui.checkbox(&mut w.invert, "Invert wheel direction");
    ui.checkbox(&mut w.drag_tunes, "Left-drag tunes as well as pans")
        .on_hover_text("Off makes left-drag pan the view only, like right-drag.");
    ui.checkbox(&mut w.digit_wheel, "Scroll a digit on the frequency readout to tune it");
    if w.wheel == WheelAction::Tune && w.wheel_shift == WheelAction::Tune {
        ui.label(
            RichText::new("Both wheel actions are Tune — there is no way left to zoom.")
                .color(Color32::from_rgb(230, 170, 60)),
        );
    }
    ui.add_space(6.0);
    if crate::chrome::chip(ui, false, "Restore mouse defaults").clicked() {
        cfg.wheel = WheelSettings::default();
    }

    ui.add_space(10.0);
    ui.separator();
    ui.add_space(6.0);
    ui.label(RichText::new("Mouse buttons").size(14.0).strong().color(crate::theme::CYAN));
    ui.add_space(4.0);
    ui.label(
        RichText::new(
            "The left and right buttons are reserved for tuning and panning; the middle and \
             extra buttons are free. A side button held for PTT behaves like a footswitch.",
        )
        .weak(),
    );
    ui.add_space(6.0);

    let mut remove: Option<usize> = None;
    egui::Grid::new("mousebtn-grid").num_columns(5).spacing([10.0, 6.0]).striped(true).show(
        ui,
        |ui| {
            for (i, b) in cfg.mouse_buttons.iter_mut().enumerate() {
                ComboBox::from_id_salt(("mb", i)).width(130.0).selected_text(b.button.label())
                    .show_ui(ui, |ui| {
                        for m in MouseButton::ALL {
                            if ui.selectable_label(b.button == m, m.label()).clicked() {
                                b.button = m;
                            }
                        }
                    });
                action_combo(ui, ("mbact", i), &mut b.action, memories);
                ui.horizontal(|ui| {
                    for m in ButtonMode::ALL {
                        if crate::chrome::chip(ui, b.button_mode == m, m.label()).clicked() {
                            b.button_mode = m;
                        }
                    }
                });
                ui.checkbox(&mut b.enabled, "");
                if ui.small_button("✕").clicked() {
                    remove = Some(i);
                }
                ui.end_row();
            }
        },
    );
    if let Some(i) = remove {
        cfg.mouse_buttons.remove(i);
    }
    ui.add_space(6.0);
    if crate::chrome::chip(ui, false, "+ Add mouse button").clicked() {
        cfg.mouse_buttons.push(MouseButtonBinding::default());
    }

    ui.add_space(8.0);
    ui.label(
        RichText::new(
            "F1 always opens this manual, even while typing, so it is not rebindable.",
        )
        .weak(),
    );

    ui.add_space(10.0);
    ui.separator();
    ui.add_space(6.0);
    settings_midi_section(ui, cfg, io.midi_learn, io.midi_rescan, memories, midi_in, midi_out,
        midi_status, last_midi);
}

/// MIDI control surfaces: port selection, a live message readout, and the
/// binding table with its LEARN capture.
#[allow(clippy::too_many_arguments)]
fn settings_midi_section(
    ui: &mut egui::Ui,
    cfg: &mut sdroxide_types::InputSettings,
    learn: &mut Option<crate::input::MidiLearn>,
    rescan: &mut bool,
    memories: &[MemoryChannel],
    midi_in: &[(String, String)],
    midi_out: &[(String, String)],
    status: &crate::input::MidiStatusView,
    last_midi: Option<(sdroxide_types::MidiMsg, u8)>,
) {
    use sdroxide_types::{ActionKind, ButtonMode, MidiBinding, RelativeMode};

    ui.label(RichText::new("MIDI controller").size(14.0).strong().color(crate::theme::CYAN));
    ui.add_space(4.0);
    if !status.supported {
        ui.label(
            RichText::new(
                "MIDI controllers need the native app — the browser client has no MIDI access.",
            )
            .weak(),
        );
        return;
    }
    ui.label(
        RichText::new(
            "Any class-compliant MIDI surface works: a DJ controller's jog wheel makes a fine              VFO knob, its pads make PTT and band buttons, its faders make gain controls.",
        )
        .weak(),
    );
    ui.add_space(6.0);
    ui.checkbox(&mut cfg.midi.enabled, "Enable");
    ui.add_space(6.0);

    ui.add_enabled_ui(cfg.midi.enabled, |ui| {
        egui::Grid::new("midi-ports").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label("Controller");
            midi_port_combo(
                ui,
                "midi-in",
                midi_in,
                &mut cfg.midi.in_port_id,
                &mut cfg.midi.in_port_name,
            );
            ui.end_row();

            ui.label("Feedback to");
            midi_port_combo(
                ui,
                "midi-out",
                midi_out,
                &mut cfg.midi.out_port_id,
                &mut cfg.midi.out_port_name,
            );
            ui.end_row();
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if crate::chrome::chip(ui, false, "Rescan ports").clicked() {
                *rescan = true;
            }
            if status.connected {
                ui.label(
                    RichText::new(format!("● {}", status.port))
                        .color(Color32::from_rgb(90, 200, 110)),
                );
            } else if let Some(e) = &status.error {
                ui.label(RichText::new(e).color(Color32::from_rgb(230, 90, 80)));
            } else {
                ui.label(RichText::new("Not connected.").weak());
            }
        });
        ui.add_space(4.0);
        // Naming the control that just moved is what makes an unlabelled
        // surface bindable at all.
        match last_midi {
            Some((msg, v)) => {
                ui.label(RichText::new(format!("Last message: {}  value {v}", msg.label())).weak())
            }
            None => ui.label(RichText::new("Move a control to see it here.").weak()),
        };
    });

    ui.add_space(8.0);
    let mut remove: Option<usize> = None;
    egui::Grid::new("midi-grid").num_columns(7).spacing([10.0, 6.0]).striped(true).show(ui, |ui| {
        ui.label(RichText::new("Control").small().weak());
        ui.label(RichText::new("Does").small().weak());
        ui.label(RichText::new("Reads as").small().weak());
        ui.label(RichText::new("Step / mode").small().weak());
        ui.label(RichText::new("LED").small().weak());
        ui.label(RichText::new("On").small().weak());
        ui.label("");
        ui.end_row();

        for (i, b) in cfg.midi.bindings.iter_mut().enumerate() {
            let learning = learn.map(|l| l.row) == Some(i);
            let label = if learning { "move it…".to_string() } else { b.msg.label() };
            if crate::chrome::chip(ui, learning, RichText::new(label).monospace())
                .on_hover_text("Click, then move the control you want to bind")
                .clicked()
            {
                *learn = if learning { None } else { Some(crate::input::MidiLearn { row: i }) };
            }

            if action_combo(ui, ("midiact", i), &mut b.action, memories) {
                b.tuning.step = b.action.default_step();
            }

            match b.action.kind() {
                ActionKind::Continuous => {
                    ComboBox::from_id_salt(("midirel", i))
                        .width(170.0)
                        .selected_text(b.relative.label())
                        .show_ui(ui, |ui| {
                            for m in RelativeMode::ALL {
                                if ui.selectable_label(b.relative == m, m.label()).clicked() {
                                    b.relative = m;
                                }
                            }
                        });
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::DragValue::new(&mut b.tuning.step)
                                .speed(1.0)
                                .range(0.0001..=1_000_000.0),
                        );
                        ui.add(
                            egui::DragValue::new(&mut b.tuning.accel)
                                .speed(0.05)
                                .range(0.0..=4.0)
                                .prefix("×"),
                        )
                        .on_hover_text("Speed sensitivity: spin faster to tune faster");
                        // Sign/magnitude and 64-centred encoders are
                        // indistinguishable from small movements, so a wrong
                        // guess shows up as a knob that turns the wrong way.
                        ui.checkbox(&mut b.tuning.invert, "rev");
                    });
                }
                ActionKind::Momentary => {
                    ui.label("");
                    ui.horizontal(|ui| {
                        for m in ButtonMode::ALL {
                            if crate::chrome::chip(ui, b.button_mode == m, m.label()).clicked() {
                                b.button_mode = m;
                            }
                        }
                    });
                }
            }

            ui.checkbox(&mut b.feedback, "")
                .on_hover_text("Send the current value back, to light an LED or move a fader");
            ui.checkbox(&mut b.enabled, "");
            if ui.small_button("✕").clicked() {
                remove = Some(i);
            }
            ui.end_row();
        }
    });
    if let Some(i) = remove {
        cfg.midi.bindings.remove(i);
        if learn.map(|l| l.row) == Some(i) {
            *learn = None;
        }
    }

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if crate::chrome::chip(ui, false, "+ Add MIDI control").clicked() {
            cfg.midi.bindings.push(MidiBinding::default());
            *learn = Some(crate::input::MidiLearn { row: cfg.midi.bindings.len() - 1 });
        }
        if !cfg.midi.bindings.is_empty()
            && crate::chrome::chip(ui, false, "Clear all").clicked()
        {
            cfg.midi.bindings.clear();
            *learn = None;
        }
    });

    ui.add_space(8.0);
    ui.label(
        RichText::new(
            "Endless (jog) encoders send a relative step rather than a position, in one of three              encodings that look alike from small movements. LEARN guesses from a clockwise turn;              if the knob then tunes the wrong way, tick \u{201c}rev\u{201d}.",
        )
        .weak(),
    );
}

/// Port dropdown that keeps both the stable id and the human name — the id
/// reconnects across a replug, the name is the label and the fallback.
fn midi_port_combo(
    ui: &mut egui::Ui,
    id: &str,
    ports: &[(String, String)],
    sel_id: &mut String,
    sel_name: &mut String,
) {
    let shown = if sel_name.is_empty() { "— none —" } else { sel_name.as_str() };
    ComboBox::from_id_salt(id).width(280.0).selected_text(shown).show_ui(ui, |ui| {
        if ui.selectable_label(sel_name.is_empty(), "— none —").clicked() {
            sel_id.clear();
            sel_name.clear();
        }
        for (pid, name) in ports {
            if ui.selectable_label(sel_name == name, name).clicked() {
                *sel_id = pid.clone();
                *sel_name = name.clone();
            }
        }
    });
}

/// Dropdown over [`WheelAction`].
fn wheel_action_combo(ui: &mut egui::Ui, id: &str, act: &mut sdroxide_types::WheelAction) {
    ComboBox::from_id_salt(id).width(130.0).selected_text(act.label()).show_ui(ui, |ui| {
        for a in sdroxide_types::WheelAction::ALL {
            if ui.selectable_label(*act == a, a.label()).clicked() {
                *act = a;
            }
        }
    });
}

/// The built-in Hamlib rigctld server: the control surface every "NET rigctl"
/// client speaks.
/// WSJT-X UDP broadcast: what the logging ecosystem listens for.
fn settings_wsjtx_tab(
    ui: &mut egui::Ui,
    cfg: &mut sdroxide_types::WsjtxConfig,
    seeded: bool,
    apply: &mut bool,
) {
    ui.label(RichText::new("WSJT-X UDP broadcast").size(14.0).strong().color(crate::theme::CYAN));
    ui.add_space(4.0);
    if !seeded {
        ui.label(
            RichText::new(
                "The broadcast leaves the machine the radio engine runs on, so it can only be \
                 configured from the native app.",
            )
            .weak(),
        );
        return;
    }
    ui.label(
        RichText::new(
            "Sends decodes, station status and logged QSOs the way WSJT-X does, so GridTracker, \
             JTAlert, N1MM+ and Log4OM work with sdroxide unchanged. Output only — nothing on \
             this socket can touch the radio.",
        )
        .weak(),
    );
    ui.add_space(6.0);
    ui.checkbox(&mut cfg.enabled, "Enable");
    ui.add_space(6.0);
    ui.add_enabled_ui(cfg.enabled, |ui| {
        egui::Grid::new("wsjtx-grid").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label("Send to");
            ui.add(
                egui::TextEdit::singleline(&mut cfg.host)
                    .desired_width(160.0)
                    .hint_text("127.0.0.1"),
            )
            .on_hover_text(
                "127.0.0.1 reaches clients on this machine; a LAN address or a multicast \
                 group (224.0.0.1) reaches others",
            );
            ui.end_row();

            ui.label("Port");
            ui.add(egui::DragValue::new(&mut cfg.port).range(1..=65535))
                .on_hover_text("2237 is the port every client defaults to");
            ui.end_row();

            ui.label("Identify as");
            ui.add(egui::TextEdit::singleline(&mut cfg.id).desired_width(160.0)).on_hover_text(
                "The name clients see. Some loggers only accept traffic identifying itself \
                 as WSJT-X.",
            );
            ui.end_row();
        });
    });

    ui.add_space(8.0);
    if crate::chrome::chip_accent(
        ui,
        false,
        RichText::new(" APPLY ").strong(),
        crate::theme::GREEN,
        crate::theme::INK_ON_CYAN,
    )
    .on_hover_text("Persist and (re)open the broadcast socket")
    .clicked()
    {
        *apply = true;
    }
}

fn settings_rigctld_tab(
    ui: &mut egui::Ui,
    cfg: &mut sdroxide_types::RigctldConfig,
    seeded: bool,
    status: &Option<TciServerStatus>,
    apply: &mut bool,
) {
    use sdroxide_types::RigctldConfig;

    ui.label(
        RichText::new("Hamlib rigctld server").size(14.0).strong().color(crate::theme::CYAN),
    );
    ui.add_space(4.0);
    if !seeded {
        ui.label(
            RichText::new(
                "The rigctld server runs alongside the radio engine, so it can only be \
                 configured from the native app.",
            )
            .weak(),
        );
        return;
    }
    ui.add_space(2.0);
    ui.checkbox(&mut cfg.enabled, "Enable");
    ui.add_space(6.0);

    ui.add_enabled_ui(cfg.enabled, |ui| {
        egui::Grid::new("rigctld-grid").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label("Listen on");
            ComboBox::from_id_salt("rigctld_bind").selected_text(&cfg.bind).show_ui(ui, |ui| {
                for b in RigctldConfig::BINDS {
                    let label = if b == "127.0.0.1" {
                        "127.0.0.1 (this machine)"
                    } else {
                        "0.0.0.0 (whole network)"
                    };
                    if ui.selectable_label(cfg.bind == b, label).clicked() {
                        cfg.bind = b.to_string();
                    }
                }
            });
            ui.end_row();

            ui.label("Port");
            ui.add(egui::DragValue::new(&mut cfg.port).range(1..=65535))
                .on_hover_text("4532 is the port every rigctld client assumes");
            ui.end_row();

            ui.label("Rig name");
            ui.add(
                egui::TextEdit::singleline(&mut cfg.rig_name)
                    .desired_width(160.0)
                    .hint_text("reported to clients"),
            );
            ui.end_row();

            ui.label("Max clients");
            ui.add(egui::DragValue::new(&mut cfg.max_clients).range(1..=32));
            ui.end_row();
        });
        ui.add_space(4.0);
        ui.checkbox(&mut cfg.allow_tx, "Allow clients to transmit").on_hover_text(
            "Off refuses every key request and stops advertising a transmit range, so Hamlib \
             itself declines to key.",
        );
    });

    // Same hazard as TCI: the protocol has no authentication at all.
    if cfg.enabled && cfg.is_open_to_network() {
        ui.add_space(6.0);
        ui.label(
            RichText::new(
                "⚠ rigctld has no authentication. On 0.0.0.0, anyone who can reach this port can \
                 tune and key your transmitter.",
            )
            .color(Color32::from_rgb(230, 170, 60)),
        );
    }

    ui.add_space(8.0);
    match status {
        Some(s) if s.running => {
            let clients = match s.clients {
                1 => "1 client".to_string(),
                n => format!("{n} clients"),
            };
            ui.label(
                RichText::new(format!("● Listening on {} — {clients}", s.addr))
                    .color(Color32::from_rgb(90, 200, 110)),
            );
        }
        Some(s) => match &s.error {
            Some(e) => {
                ui.label(RichText::new(format!("Failed: {e}")).color(Color32::from_rgb(230, 90, 80)));
            }
            None => {
                ui.label(RichText::new("Not running.").weak());
            }
        },
        None => {
            ui.label(RichText::new("Status unknown — press APPLY.").weak());
        }
    }

    ui.add_space(8.0);
    if crate::chrome::chip_accent(
        ui,
        false,
        RichText::new(" APPLY ").strong(),
        crate::theme::GREEN,
        crate::theme::INK_ON_CYAN,
    )
    .on_hover_text("Persist and (re)bind the server")
    .clicked()
    {
        *apply = true;
    }

    ui.add_space(8.0);
    ui.label(
        RichText::new(
            "Lets any Hamlib-capable program drive this radio: frequency, mode, PTT, split, RIT \
             and power. In WSJT-X, fldigi or CQRLOG choose the rig \u{201c}Hamlib NET rigctl\u{201d} \
             (model 2) and point it at this address; in GPredict and N1MM enter the host and port \
             directly. Unlike the TCI server it carries control only \u{2014} no audio, no IQ.",
        )
        .weak(),
    );
}

fn settings_tci_server_tab(
    ui: &mut egui::Ui,
    cfg: &mut sdroxide_types::TciServerConfig,
    seeded: bool,
    status: &Option<TciServerStatus>,
    apply: &mut bool,
) {
    use sdroxide_types::TciServerConfig;

    if !seeded {
        ui.label(
            RichText::new("The TCI server runs alongside the radio engine, so it can only be \
                           configured from the native app.")
                .weak(),
        );
        return;
    }

    ui.label(RichText::new("Built-in TCI server").size(14.0).strong().color(crate::theme::CYAN));
    ui.add_space(6.0);
    ui.checkbox(&mut cfg.enabled, "Enable");
    ui.add_space(6.0);

    ui.add_enabled_ui(cfg.enabled, |ui| {
        egui::Grid::new("tci-srv-grid").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label("Listen on");
            ComboBox::from_id_salt("tci_srv_bind").selected_text(&cfg.bind).show_ui(ui, |ui| {
                for b in TciServerConfig::BINDS {
                    let label = if b == "127.0.0.1" {
                        "127.0.0.1 (this machine)"
                    } else {
                        "0.0.0.0 (whole network)"
                    };
                    if ui.selectable_label(cfg.bind == b, label).clicked() {
                        cfg.bind = b.to_string();
                    }
                }
            });
            ui.end_row();

            ui.label("Port");
            ui.add(egui::DragValue::new(&mut cfg.port).range(1..=65535));
            ui.end_row();

            ui.label("Device name");
            ui.add(
                egui::TextEdit::singleline(&mut cfg.device_name)
                    .desired_width(160.0)
                    .hint_text("reported to clients"),
            );
            ui.end_row();

            ui.label("Max clients");
            ui.add(egui::DragValue::new(&mut cfg.max_clients).range(1..=32));
            ui.end_row();
        });
        ui.add_space(4.0);
        ui.checkbox(&mut cfg.allow_tx, "Allow clients to transmit").on_hover_text(
            "Off leaves control and the receive streams working, but every key request \
             is refused.",
        );
    });

    // Security: TCI has no authentication at all, so binding wide open hands
    // the transmitter to anyone who can reach the port.
    if cfg.enabled && cfg.is_open_to_network() {
        ui.add_space(6.0);
        ui.label(
            RichText::new(
                "⚠ TCI has no authentication. On 0.0.0.0, anyone who can reach this port can \
                 tune and key your transmitter.",
            )
            .color(Color32::from_rgb(230, 170, 60)),
        );
    }

    ui.add_space(8.0);
    match status {
        Some(s) if s.running => {
            let clients = match s.clients {
                1 => "1 client".to_string(),
                n => format!("{n} clients"),
            };
            ui.label(
                RichText::new(format!("● Listening on {} — {clients}", s.addr))
                    .color(Color32::from_rgb(90, 200, 110)),
            );
        }
        Some(s) => match &s.error {
            Some(e) => {
                let msg = RichText::new(format!("Failed: {e}"));
                ui.label(msg.color(Color32::from_rgb(230, 90, 80)));
            }
            None => {
                ui.label(RichText::new("Not running.").weak());
            }
        },
        None => {
            ui.label(RichText::new("Status unknown — press APPLY.").weak());
        }
    }

    ui.add_space(8.0);
    if crate::chrome::chip_accent(
        ui,
        false,
        RichText::new(" APPLY ").strong(),
        crate::theme::GREEN,
        crate::theme::INK_ON_CYAN,
    )
    .on_hover_text("Persist and (re)bind the server")
    .clicked()
    {
        *apply = true;
    }

    ui.add_space(8.0);
    ui.label(
        RichText::new(
            "Lets TCI-capable programs drive this radio: frequency and mode control, wideband IQ, \
             receive audio, and transmit audio. In WSJT-X choose the SunSDR (TCI) rig type, point \
             it at this address, and set PTT to TCI.",
        )
        .weak(),
    );
}

impl eframe::App for SdroxideApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let now = ctx.input(|i| i.time);
        while let Some(ev) = self.ctrl.poll_event() {
            match ev {
                RadioEvent::Capabilities(c) => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!(
                        "sdroxide — {}",
                        c.label
                    )));
                    self.caps = Some(c);
                }
                RadioEvent::State(s) => {
                    let prev_vfo = self.state.active_freq_hz();
                    let prev_mode = self.state.rx[0].mode;
                    let prev_span = self.state.sample_rate;
                    self.state = s;
                    if self.state.rx[0].mode != prev_mode {
                        self.clear_digi_rx();
                    }
                    // Follow the device when it changes how much it can show —
                    // an Icom's scope span switched on the radio, a new
                    // interface, a different sample rate. Keeping the old zoom
                    // would leave a ±250 kHz scope drawn a few kHz wide, and
                    // nothing else refits the view: that is the F key's job,
                    // and the operator should not have to press it because the
                    // radio changed underneath them.
                    if (self.state.sample_rate - prev_span).abs() > 1.0
                        && self.state.sample_rate > 0.0
                        && !self.view.is_unset()
                    {
                        self.view.fit(self.state.center_hz, self.state.sample_rate);
                    }
                    self.recenter_if_tuned_away(prev_vfo);
                }
                RadioEvent::Spectrum(f) => {
                    self.frame = Some(std::sync::Arc::new(f));
                    self.last_spectrum_at = now;
                }
                RadioEvent::Meters(m) => self.meters = Some(m),
                RadioEvent::Memories(m) => self.memories = m,
                RadioEvent::ConnectionLost(e) => self.error = Some(e),
                RadioEvent::Notice(n) => self.radio_notice = n,
                RadioEvent::Ft8Decodes(d) => {
                    // Prepend newest-slot decodes; keep a rolling window.
                    for dec in d.into_iter().rev() {
                        self.digi_decodes.insert(0, dec);
                    }
                    self.digi_decodes.truncate(200);
                }
                RadioEvent::Ft8Status(s) => {
                    // Seed the editable config from the engine's persisted
                    // value once (later edits are UI-owned so typing sticks).
                    if !self.digi_cfg_seeded {
                        self.digi_cfg_edit = s.config.clone();
                        self.digi_cfg_seeded = true;
                    }
                    self.digi_status = Some(s);
                }
                RadioEvent::Ft8QsoLogged(mut r) => {
                    r.id = self.next_log_id();
                    let call = r.call.clone();
                    let adif = auto_upload_adif(&self.net_cfg_edit, &r);
                    self.qso_log.push(r);
                    persist_qso_log(&self.qso_log);
                    // Enrich + optionally upload the freshly logged QSO.
                    self.queue_lookup(call);
                    if let Some((qso_id, adif, targets)) = adif {
                        self.pending_uploads.push((qso_id, adif, targets));
                    }
                }
                RadioEvent::SstvLine { image_id, y, rgb } => {
                    self.sstv.on_line(image_id, y, &rgb, &ctx);
                }
                RadioEvent::SstvImage { image_id, mode, w, h, png } => {
                    self.sstv.on_image(image_id, mode, w, h, &png, &ctx);
                }
                RadioEvent::DigiImage { png } => {
                    if let Some((rgb, w, h)) = crate::sstv::decode_image(&png) {
                        let ci = crate::sstv::color_image(&rgb, w, h);
                        let tex = ctx.load_texture("fsq_rx", ci, egui::TextureOptions::LINEAR);
                        self.fsq_rx_images.insert(0, tex);
                        self.fsq_rx_images.truncate(30);
                    }
                }
                RadioEvent::SstvStatus(s) => {
                    // Adopt a *newly* detected RX mode for the next transmit, but
                    // don't re-apply a steady detection every frame — that would
                    // fight the operator's manual mode selection.
                    if s.detected != self.sstv.last_detected {
                        if let Some(m) = s.detected {
                            self.sstv.tx_mode = m;
                            self.sstv.preview_dirty = true;
                        }
                        self.sstv.last_detected = s.detected;
                    }
                    self.sstv.status = s;
                }
                RadioEvent::SkimmerSpots(s) => {
                    // The engine sends the full current set each update; the
                    // stable `id` per spot lets the overlay keep each box (and
                    // its scroll) in place across updates.
                    for spot in &s {
                        // Remember when each spot last keyed, and seed newly
                        // seen ones to now, so alpha starts solid and fades.
                        let e = self.skimmer_active_at.entry(spot.id).or_insert(now);
                        if spot.active {
                            *e = now;
                        }
                    }
                    // Forget timings for spots the engine has dropped.
                    let live: std::collections::HashSet<u64> = s.iter().map(|x| x.id).collect();
                    self.skimmer_active_at.retain(|id, _| live.contains(id));
                    self.skimmer_spots = s;
                }
                RadioEvent::Spots(s) => self.spots = s,
                RadioEvent::NetStatus(s) => self.net_status = s,
                RadioEvent::TciServerStatus { running, addr, clients, error } => {
                    self.tci_srv_status = Some(TciServerStatus { running, addr, clients, error });
                }
                RadioEvent::RigctldStatus { running, addr, clients, error } => {
                    self.rigctld_status = Some(TciServerStatus { running, addr, clients, error });
                }
                RadioEvent::VoiceStatus(v) => self.voice = v,
                RadioEvent::CallsignResult(info) => self.apply_callsign(info),
                RadioEvent::Upload(r) => self.on_upload_result(r),
                RadioEvent::Confirmations(recs) => self.apply_confirmations(recs),
            }
        }
        // A switched-off skimmer stops emitting, so its last boxes would sit on
        // the waterfall until something else replaced them; drop them per kind.
        if !self.skimmer_spots.is_empty() {
            self.skimmer_spots.retain(|s| self.state.skimmer.enabled(s.kind));
        }
        self.poll_adif_import();

        let mut cmds = Vec::new();
        // F1 toggles the manual — handled here (not in `keyboard_shortcuts`) so
        // it works even while a text field has focus.
        if ctx.input(|i| i.key_pressed(egui::Key::F1)) {
            self.help.open = !self.help.open;
        }
        self.control_inputs(&ctx, &mut cmds);
        // Shutting down with a bound key or footswitch still held would
        // otherwise leave the rig transmitting.
        if ctx.input(|i| i.viewport().close_requested()) && self.input.any_held() {
            self.release_held_controls(&mut cmds);
        }

        egui::Panel::top(egui::Id::new("topbar"))
            .frame(
                egui::Frame::new()
                    .fill(crate::theme::BG_DEEP)
                    .inner_margin(egui::Margin::symmetric(8, 6)),
            )
            .show(ui, |ui| {
                crate::chrome::angled_frame(ui, crate::theme::PINK, |ui| {
                    self.top_bar(ui, &mut cmds);
                });
            });
        // A persistent radio-audio warning (input unavailable / mono-for-IQ)
        // rides above the panadapter with a dismiss button, so a silent RX
        // failure is explained rather than reading as "waiting for spectrum".
        if let Some(notice) = self.radio_notice.clone() {
            egui::Frame::new()
                .fill(Color32::from_rgb(60, 45, 10))
                .stroke(egui::Stroke::new(1.0, Color32::from_rgb(210, 160, 40)))
                .inner_margin(egui::Margin::symmetric(8, 5))
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            RichText::new("⚠")
                                .size(15.0)
                                .color(Color32::from_rgb(255, 190, 70)),
                        );
                        ui.label(
                            RichText::new(notice).size(13.0).color(Color32::from_rgb(240, 220, 180)),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Dismiss").clicked() {
                                self.radio_notice = None;
                            }
                        });
                    });
                });
        }
        // Network-spot overlay (shared by voice + digital panadapter paths). A
        // clicked spot is captured here and pre-filled into a log entry below.
        let (net_spots, net_alpha) = self.net_overlay(now_unix());
        let mut clicked_spot: Option<Spot> = None;
        // Remaining space: the panadapter (+ FT8/FT4 operating panel).
        if let Some(err) = self.error.clone() {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new(err).size(18.0).color(Color32::RED));
            });
        } else if self.state.rx[0].mode.is_digital() {
            // Remember the voice-mode view once, so leaving FT8 can restore it
            // instead of leaving the panadapter zoomed to the sub-band.
            if self.pre_digi_view.is_none() {
                self.pre_digi_view = Some((self.view.view_lo_hz, self.view.view_hi_hz));
            }
            // Lock the view to the digital sub-band (audio 0..3.5 kHz above dial).
            let dial = self.state.rx_freq_hz();
            if self.state.rx[0].mode.is_rf_paint() {
                // Zoom tight onto the 300..3300 Hz painting band so incoming
                // pictures are large enough to read on the waterfall.
                self.view.view_lo_hz = dial + 150.0;
                self.view.view_hi_hz = dial + 3450.0;
            } else {
                self.view.view_lo_hz = dial - 200.0;
                self.view.view_hi_hz = dial + 3500.0;
            }
            let audio_hz = self.digi_status.as_ref().map(|s| s.audio_hz).unwrap_or(1500.0);
            let mode = self.state.rx[0].mode;
            let is_text = mode.is_text_modem();
            // RTTY shows mark/space tuning lines; Olivia the tone-bank edges;
            // PSK just the centre marker.
            let markers: Vec<f32> = if mode == Mode::Rtty {
                let sh = self.digi_status.as_ref().map(|s| s.config.rtty_shift_hz).unwrap_or(170.0);
                vec![audio_hz - sh / 2.0, audio_hz + sh / 2.0]
            } else if mode == Mode::Olivia {
                let bw = self.digi_status.as_ref().map(|s| s.config.olivia_bw_hz).unwrap_or(1000.0);
                vec![audio_hz - bw / 2.0, audio_hz + bw / 2.0]
            } else if mode == Mode::Thor {
                let baud =
                    self.digi_status.as_ref().map(|s| s.config.thor_mode.baud()).unwrap_or(15.625);
                let bw = 18.0 * baud;
                vec![audio_hz - bw / 2.0, audio_hz + bw / 2.0]
            } else if mode == Mode::Fsq {
                let baud = self.digi_status.as_ref().map(|s| s.config.fsq_baud).unwrap_or(4.5);
                let bw = 33.0 * baud;
                vec![audio_hz - bw / 2.0, audio_hz + bw / 2.0]
            } else if mode == Mode::RfPaint {
                // The painting band edges (300..3300 Hz).
                vec![300.0, 3300.0]
            } else if mode == Mode::Rade {
                // The RADE V1 OFDM carriers, so the operator can see whether the
                // signal is sitting inside the modem's window.
                vec![1062.0, 1876.0]
            } else {
                Vec::new()
            };
            // FT8 station callsign boxes (built before the &mut self borrows).
            // Only the slotted modes have them — SSTV / RF Paint share the digi
            // path but must not inherit FT8's overlay.
            let (ft8_spots, ft8_alpha) =
                if mode.is_slotted() { self.ft8_overlay() } else { (Vec::new(), Vec::new()) };

            let frame = self.frame.take();
            // Manual vertical split with a draggable divider: the operating
            // panel gets `digi_panel_fraction` of the height, the waterfall the
            // rest. A thin handle between them resizes the split.
            let total = ui.available_height();
            let width = ui.available_width();
            let handle_h = 7.0;
            let panel_h =
                (total * self.view.digi_panel_fraction).clamp(190.0, (total - 140.0).max(190.0));
            let wf_h = (total - panel_h - handle_h).max(80.0);

            let wf_tuning = self.wf_tick(frame.is_some());
            ui.allocate_ui(egui::vec2(width, wf_h), |ui| {
                spectrum_view::show_ext(
                    ui,
                    &mut self.view,
                    &mut self.state,
                    frame.as_ref(),
                    &mut self.peaks,
                    &mut self.spec_smooth,
                    &mut self.trace_cache,
                    Some(audio_hz),
                    &markers,
                    &ft8_spots,
                    &ft8_alpha,
                    &net_spots,
                    &net_alpha,
                    &mut clicked_spot,
                    self.input.cfg.wheel,
                    wf_tuning,
                    &mut cmds,
                );
            });
            // Resize handle between the waterfall and the FT8/FT4 panel.
            let (hrect, hresp) =
                ui.allocate_exact_size(egui::vec2(width, handle_h), egui::Sense::click_and_drag());
            if hresp.hovered() || hresp.dragged() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
            }
            if hresp.dragged() {
                // Drag down shrinks the panel (waterfall grows), drag up grows it.
                let d = hresp.drag_delta().y / total;
                self.view.digi_panel_fraction =
                    (self.view.digi_panel_fraction - d).clamp(0.2, 0.82);
            }
            {
                let p = ui.painter_at(hrect);
                let hot = hresp.hovered() || hresp.dragged();
                p.rect_filled(hrect, 0.0, crate::theme::PANEL);
                let col = if hot { crate::theme::CYAN } else { Color32::from_gray(70) };
                let cx = hrect.center().x;
                let cy = hrect.center().y;
                for dx in [-16.0f32, 0.0, 16.0] {
                    p.line_segment(
                        [egui::pos2(cx + dx - 6.0, cy), egui::pos2(cx + dx + 6.0, cy)],
                        egui::Stroke::new(2.0, col),
                    );
                }
            }
            ui.allocate_ui(egui::vec2(width, panel_h), |ui| {
                egui::Frame::new()
                    .fill(crate::theme::BG_DEEP)
                    .inner_margin(egui::Margin { left: 0, right: 0, top: 6, bottom: 0 })
                    .show(ui, |ui| {
                        crate::chrome::angled_frame(ui, crate::theme::PINK, |ui| {
                            if mode.is_rade() {
                                self.rade_panel(ui, &mut cmds, panel_h);
                            } else if mode.is_sstv() {
                                self.sstv_panel(ui, &mut cmds);
                            } else if mode.is_rf_paint() {
                                self.rf_paint_panel(ui, &mut cmds, panel_h);
                            } else if mode.is_fsq() {
                                self.fsq_panel(ui, &mut cmds, panel_h);
                            } else if is_text {
                                self.text_modem_panel(ui, &mut cmds, panel_h);
                            } else {
                                self.digi_panel(ui, &mut cmds);
                            }
                        });
                    });
            });
            self.frame = frame;
        } else {
            // Restore the pre-FT8 view span once, on the first voice frame
            // after leaving a digital mode.
            if let Some((lo, hi)) = self.pre_digi_view.take() {
                self.view.view_lo_hz = lo;
                self.view.view_hi_hz = hi;
            }
            let (cw_spots, cw_alpha) = self.cw_overlay(now);
            let frame = self.frame.take();
            let wf_tuning = self.wf_tick(frame.is_some());
            spectrum_view::show(
                ui,
                &mut self.view,
                &mut self.state,
                frame.as_ref(),
                &mut self.peaks,
                &mut self.spec_smooth,
                &mut self.trace_cache,
                &cw_spots,
                &cw_alpha,
                &net_spots,
                &net_alpha,
                &mut clicked_spot,
                self.input.cfg.wheel,
                wf_tuning,
                &mut cmds,
            );
            self.frame = frame;
        }
        // A spot clicked on the panadapter: pre-fill a log entry (tuning + mode
        // were already issued inside the widget).
        if let Some(spot) = clicked_spot {
            self.prefill_from_spot(&spot);
        }

        self.memories_window(&ctx, &mut cmds);
        self.voice_window(&ctx, &mut cmds);
        self.settings_window(&ctx, &mut cmds);
        self.digi_settings_window(&ctx, &mut cmds);
        self.logbook_window(&ctx);
        self.spots_window(&ctx, &mut cmds);
        self.awards_window(&ctx);
        self.help.ui(&ctx);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let grid = self.my_grid();
            let traffic = self.digi_traffic(ctx.input(|i| i.time));
            self.solar.viewport(&ctx, &grid, traffic);
            self.view.solar3d = self.solar.persisted();
        }

        // Debounced spectrum-config updates with pan hysteresis.
        let now = ctx.input(|i| i.time);
        if !self.cfg_still_good() {
            let ideal = self.desired_spectrum_cfg();
            if self.desired_cfg != Some(ideal) {
                self.desired_cfg = Some(ideal);
                self.desired_at = now;
            }
            if self.sent_cfg.is_none() || now - self.desired_at >= CFG_DEBOUNCE_S {
                self.sent_cfg = Some(ideal);
                cmds.push(Command::SetSpectrumCfg(ideal));
            }
        }

        // Flush queued lookups / uploads accumulated during window rendering.
        for call in std::mem::take(&mut self.pending_lookups) {
            cmds.push(Command::LookupCallsign { call });
        }
        for (qso_id, adif, targets) in std::mem::take(&mut self.pending_uploads) {
            cmds.push(Command::UploadQso { qso_id, adif, targets });
        }

        for c in cmds {
            self.ctrl.send(c);
        }

        // Data-driven repaint: redraw immediately when data is already waiting
        // (arrived while this frame was being built — checked after the drain,
        // so this can't busy-loop), otherwise wake at the next expected
        // spectrum frame, or idle-poll when nothing is streaming. User input
        // wakes eframe by itself, so interactivity is unaffected.
        if self.ctrl.wants_repaint_soon() {
            ctx.request_repaint();
        } else {
            let fps = self
                .sent_cfg
                .or(self.desired_cfg)
                .map(|c| c.fps)
                .unwrap_or(SpectrumConfig::default().fps)
                .max(1) as u64;
            let streaming = self.frame.is_some()
                && self.error.is_none()
                && now - self.last_spectrum_at < STREAM_STALE_S;
            // Floor division keeps the poll period <= the stream period, so no
            // frame is ever skipped (the spectrum buffer is latest-wins).
            let wait_ms = if streaming { 1000 / fps } else { IDLE_POLL_MS };
            ctx.request_repaint_after(Duration::from_millis(wait_ms));
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, "view", &self.view);
        // On wasm this is the logbook's persistence; on native it's a harmless
        // backup (the authoritative copy is written to the config dir on change).
        eframe::set_value(storage, "qso_log", &self.qso_log);
        // Same split: authoritative on native is config.toml (written on change).
        eframe::set_value(storage, "ui_settings", &self.ui_settings);
        // Control-input bindings: authoritative on native is input.json.
        eframe::set_value(storage, "input", &self.input.cfg);
    }
}

// ── Logbook persistence (native: config-dir JSON; wasm: eframe storage) ──────
#[cfg(not(target_arch = "wasm32"))]
fn load_qso_log(_storage: Option<&dyn eframe::Storage>) -> Vec<QsoRecord> {
    sdroxide_config::load_qso_log()
}
#[cfg(target_arch = "wasm32")]
fn load_qso_log(storage: Option<&dyn eframe::Storage>) -> Vec<QsoRecord> {
    storage.and_then(|s| eframe::get_value(s, "qso_log")).unwrap_or_default()
}

#[cfg(not(target_arch = "wasm32"))]
fn persist_qso_log(log: &[QsoRecord]) {
    if let Err(e) = sdroxide_config::save_qso_log(log) {
        eprintln!("failed to save logbook: {e}");
    }
}
#[cfg(target_arch = "wasm32")]
fn persist_qso_log(_log: &[QsoRecord]) {
    // Written by eframe's periodic `save()` into localStorage.
}

// ── UI/display preferences (native: config.toml [ui]; wasm: eframe storage) ──
#[cfg(not(target_arch = "wasm32"))]
fn load_ui_settings(_storage: Option<&dyn eframe::Storage>) -> sdroxide_types::UiSettings {
    sdroxide_config::load_ui_settings()
}
#[cfg(target_arch = "wasm32")]
fn load_ui_settings(storage: Option<&dyn eframe::Storage>) -> sdroxide_types::UiSettings {
    storage.and_then(|s| eframe::get_value(s, "ui_settings")).unwrap_or_default()
}

#[cfg(not(target_arch = "wasm32"))]
fn persist_ui_settings(ui: &sdroxide_types::UiSettings) {
    if let Err(e) = sdroxide_config::save_ui_settings(ui) {
        eprintln!("failed to save UI settings: {e}");
    }
}
#[cfg(target_arch = "wasm32")]
fn persist_ui_settings(_ui: &sdroxide_types::UiSettings) {
    // Written by eframe's periodic `save()` into localStorage.
}

use crate::time::{now_unix, now_unix_f64};

/// Parse `"YYYY-MM-DD"` + `"HH:MM"` (UTC) to a Unix timestamp, falling back to
/// `fallback` if the fields don't fully parse.
fn parse_utc(date: &str, time: &str, fallback: i64) -> i64 {
    let d: Vec<&str> = date.trim().split('-').collect();
    let t: Vec<&str> = time.trim().split(':').collect();
    if d.len() == 3 && t.len() >= 2 {
        if let (Ok(y), Ok(mo), Ok(day), Ok(h), Ok(mi)) =
            (d[0].parse(), d[1].parse(), d[2].parse(), t[0].parse(), t[1].parse())
        {
            return sdroxide_types::ymd_hms_to_unix(y, mo, day, h, mi, 0);
        }
    }
    fallback
}

/// `"YYYY-MM-DD"` for a Unix timestamp (UTC).
fn date_str(unix: i64) -> String {
    let (y, mo, d, ..) = sdroxide_types::utc_ymd_hms(unix);
    format!("{y:04}-{mo:02}-{d:02}")
}

/// `"HH:MM"` for a Unix timestamp (UTC).
fn time_str(unix: i64) -> String {
    let (_, _, _, h, mi, _) = sdroxide_types::utc_ymd_hms(unix);
    format!("{h:02}:{mi:02}")
}

/// Compact age of a spot: `"12s"`, `"3m"`, `"1h"`.
fn fmt_age(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else {
        format!("{}h", s / 3600)
    }
}

/// One clickable spot row for the spots window: kind badge, call, frequency,
/// mode, age, and the park/summit reference or comment.
fn spot_row(ui: &mut egui::Ui, s: &Spot, now_utc: i64, needed: bool) -> egui::Response {
    let (r, g, b) = s.kind.color();
    let kind_col = Color32::from_rgb(r, g, b);
    let gray = Color32::from_gray(150);
    let inner = egui::Frame::new()
        .fill(crate::theme::ROW_BG)
        .inner_margin(egui::Margin { left: 8, right: 6, top: 4, bottom: 4 })
        .show(ui, |ui| {
            ui.set_min_height(22.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let col = |ui: &mut egui::Ui, w: f32, lbl: egui::Label| {
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(w, 20.0), egui::Sense::hover());
                    ui.new_child(
                        egui::UiBuilder::new()
                            .max_rect(rect)
                            .layout(egui::Layout::left_to_right(egui::Align::Center)),
                    )
                    .add(lbl);
                };
                col(ui, 44.0, egui::Label::new(RichText::new(s.kind.label()).size(10.0).strong().color(kind_col)));
                col(
                    ui,
                    90.0,
                    egui::Label::new(
                        RichText::new(&s.call).size(14.0).strong().color(crate::theme::TEXT_STRONG),
                    )
                    .truncate(),
                );
                col(
                    ui,
                    78.0,
                    egui::Label::new(
                        RichText::new(format!("{:.4}", s.freq_hz / 1e6)).monospace().size(12.0).color(gray),
                    ),
                );
                col(ui, 46.0, egui::Label::new(RichText::new(&s.mode).monospace().size(11.0).color(gray)));
                col(ui, 34.0, egui::Label::new(RichText::new(fmt_age(now_utc - s.when_utc)).size(10.5).color(Color32::from_gray(120))));
                if needed {
                    col(
                        ui,
                        36.0,
                        egui::Label::new(
                            RichText::new("NEW").size(10.0).strong().color(crate::theme::GREEN),
                        ),
                    );
                }
                let info = match &s.reference {
                    Some(r) if !s.comment.is_empty() => format!("{r} · {}", s.comment),
                    Some(r) => r.clone(),
                    None => s.comment.clone(),
                };
                ui.add(egui::Label::new(RichText::new(info).size(11.0).color(gray)).truncate());
            });
        });
    let resp = inner.response.interact(egui::Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

/// One "N worked / M confirmed" summary line for an award map.
fn award_summary<K>(
    ui: &mut egui::Ui,
    name: &str,
    map: &std::collections::BTreeMap<K, sdroxide_types::AwardStatus>,
) {
    let (w, c) = sdroxide_types::counts(map);
    ui.horizontal(|ui| {
        ui.add_sized([90.0, 20.0], egui::Label::new(RichText::new(name).strong()));
        ui.label(RichText::new(format!("{w} worked")).color(crate::theme::YELLOW).monospace());
        ui.label(RichText::new(format!("{c} confirmed")).color(crate::theme::GREEN).monospace());
    });
}

/// A wrapping grid of fixed-width award cells: grey = not worked, amber =
/// worked, green = confirmed.
fn award_cell_grid(
    ui: &mut egui::Ui,
    items: impl Iterator<Item = (String, sdroxide_types::AwardStatus)>,
    cell_w: f32,
) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
        for (label, st) in items {
            let (bg, fg) = if st.confirmed {
                (crate::theme::GREEN, Color32::from_rgb(8, 18, 12))
            } else if st.worked {
                (crate::theme::YELLOW, Color32::from_rgb(20, 16, 6))
            } else {
                (Color32::from_gray(38), Color32::from_gray(110))
            };
            let (rect, _) = ui.allocate_exact_size(egui::vec2(cell_w, 20.0), egui::Sense::hover());
            let p = ui.painter_at(rect);
            p.rect_filled(rect, 3.0, bg);
            p.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                label,
                egui::FontId::monospace(11.0),
                fg,
            );
        }
    });
}

/// Standard FT8/FT4 dial frequencies per HF/6 m band.
/// The standard FT8/FT4 dial frequency for `band`, if one exists for `mode`
/// (matched by which band's edges the frequency falls within).
fn digi_freq_for_band(mode: Mode, band: Band) -> Option<f64> {
    let (lo, hi) = band.edges()?;
    digi_dial_freqs(mode).iter().find(|&&(_, hz)| (lo..=hi).contains(&hz)).map(|&(_, hz)| hz)
}

fn digi_dial_freqs(mode: Mode) -> &'static [(&'static str, f64)] {
    match mode {
        // PSK31 activity centres (USB dial; signals sit ~1 kHz above).
        Mode::Psk => &[
            ("80m", 3_580_000.0),
            ("40m", 7_040_000.0),
            ("30m", 10_142_000.0),
            ("20m", 14_070_000.0),
            ("17m", 18_097_000.0),
            ("15m", 21_070_000.0),
            ("12m", 24_920_000.0),
            ("10m", 28_120_000.0),
        ],
        // RTTY sub-band starts (USB dial).
        Mode::Rtty => &[
            ("80m", 3_580_000.0),
            ("40m", 7_040_000.0),
            ("30m", 10_140_000.0),
            ("20m", 14_080_000.0),
            ("17m", 18_100_000.0),
            ("15m", 21_080_000.0),
            ("12m", 24_920_000.0),
            ("10m", 28_080_000.0),
        ],
        Mode::Ft4 => &[
            ("80m", 3_575_000.0),
            ("40m", 7_047_500.0),
            ("30m", 10_140_000.0),
            ("20m", 14_080_000.0),
            ("17m", 18_104_000.0),
            ("15m", 21_140_000.0),
            ("12m", 24_919_000.0),
            ("10m", 28_180_000.0),
        ],
        // FreeDV calling frequencies (USB dial).
        Mode::Rade => &[
            ("80m", 3_625_000.0),
            ("40m", 7_177_000.0),
            ("20m", 14_236_000.0),
            ("15m", 21_313_000.0),
            ("10m", 28_330_000.0),
        ],
        // SSTV calling frequencies (USB).
        Mode::Sstv => &[
            ("80m", 3_730_000.0),
            ("40m", 7_171_000.0),
            ("20m", 14_230_000.0),
            ("15m", 21_340_000.0),
            ("10m", 28_680_000.0),
        ],
        // Olivia activity centres (USB dial).
        Mode::Olivia => &[
            ("80m", 3_581_000.0),
            ("40m", 7_073_000.0),
            ("30m", 10_142_000.0),
            ("20m", 14_076_000.0),
            ("17m", 18_103_000.0),
            ("15m", 21_076_000.0),
            ("10m", 28_076_000.0),
        ],
        // THOR / DominoEX activity centres (USB dial).
        Mode::Thor => &[
            ("80m", 3_580_000.0),
            ("40m", 7_070_000.0),
            ("30m", 10_147_000.0),
            ("20m", 14_073_000.0),
            ("17m", 18_103_000.0),
            ("15m", 21_073_000.0),
            ("10m", 28_073_000.0),
        ],
        // FSQCALL calling frequencies (USB dial; signals ~1500 Hz above).
        Mode::Fsq => &[
            ("80m", 3_575_000.0),
            ("40m", 7_105_000.0),
            ("30m", 10_144_000.0),
            ("20m", 14_105_000.0),
            ("17m", 18_104_000.0),
            ("15m", 21_105_000.0),
            ("10m", 28_105_000.0),
        ],
        // RF Paint has no defined calling frequency — offer no band presets.
        Mode::RfPaint => &[],
        // FT8 (and default).
        _ => &[
            ("160m", 1_840_000.0),
            ("80m", 3_573_000.0),
            ("40m", 7_074_000.0),
            ("30m", 10_136_000.0),
            ("20m", 14_074_000.0),
            ("17m", 18_100_000.0),
            ("15m", 21_074_000.0),
            ("12m", 24_915_000.0),
            ("10m", 28_074_000.0),
            ("6m", 50_313_000.0),
        ],
    }
}

/// Pick `(floor, ceil)` dB for best waterfall contrast from a frame's u8
/// `bins` (mapped over `[db_floor, db_ceil]`). Percentile-based so a single
/// strong carrier doesn't over-blow the scale and weak signals stay visible.
/// Returns `None` for an empty or degenerate frame.
fn pick_levels(bins: &[u8], db_floor: f32, db_ceil: f32) -> Option<(f32, f32)> {
    let range = db_ceil - db_floor;
    if bins.is_empty() || range <= 0.0 {
        return None;
    }
    // Reconstruct approximate dB per bin from the u8 mapping and sort.
    let mut db: Vec<f32> = bins.iter().map(|&b| db_floor + (b as f32 / 255.0) * range).collect();
    db.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let pct = |p: f32| -> f32 {
        let i = ((p * (db.len() - 1) as f32).round() as usize).min(db.len() - 1);
        db[i]
    };
    let noise = pct(0.25); // typical noise floor
    let peak = pct(0.99); // strong signals, ignoring the hottest outliers
    let mut floor = noise - 5.0; // noise sits just above the floor (dark)
    let mut ceil = peak + 6.0; // headroom so strong signals don't clip
    // Keep a usable dynamic range even on an empty/flat band.
    let min_range = 24.0;
    if ceil - floor < min_range {
        let mid = 0.5 * (ceil + floor);
        floor = mid - 0.5 * min_range;
        ceil = mid + 0.5 * min_range;
    }
    // Clamp to the same bounds as the manual controls.
    let floor = floor.clamp(-160.0, -40.0);
    let mut ceil = ceil.clamp(-100.0, 20.0);
    if ceil - floor < 10.0 {
        ceil = (floor + 10.0).min(20.0);
    }
    Some((floor, ceil))
}

/// Colour a decode's SNR: green for strong, cyan mid, dimmed for weak.
fn snr_color(snr_db: i16) -> Color32 {
    if snr_db >= 0 {
        crate::theme::GREEN
    } else if snr_db >= -12 {
        crate::theme::CYAN
    } else {
        crate::theme::CYAN_DIM
    }
}

#[cfg(test)]
mod tests {
    use super::pick_levels;

    /// Map a dB value to the u8 code used by a frame spanning `[lo, hi]`.
    fn code(db: f32, lo: f32, hi: f32) -> u8 {
        (((db - lo) / (hi - lo) * 255.0).clamp(0.0, 255.0)) as u8
    }

    #[test]
    fn levels_bracket_noise_and_signals() {
        // Frame mapped over a wide [-120, -20]: mostly noise near -110 with a
        // handful of strong signals near -45.
        let (lo, hi) = (-120.0f32, -20.0f32);
        let mut bins = vec![code(-110.0, lo, hi); 1000];
        bins.extend(std::iter::repeat(code(-45.0, lo, hi)).take(20));
        let (floor, ceil) = pick_levels(&bins, lo, hi).unwrap();
        // Floor just below the noise; ceiling just above the signals.
        assert!((-120.0..-100.0).contains(&floor), "floor {floor}");
        assert!((-55.0..-30.0).contains(&ceil), "ceil {ceil}");
        assert!(ceil - floor >= 24.0, "range {}", ceil - floor);
    }

    #[test]
    fn flat_band_keeps_minimum_range() {
        // A noise-only band still gets a usable contrast window, not a sliver.
        let (lo, hi) = (-120.0f32, -20.0f32);
        let bins = vec![code(-108.0, lo, hi); 512];
        let (floor, ceil) = pick_levels(&bins, lo, hi).unwrap();
        assert!(ceil - floor >= 24.0, "range {}", ceil - floor);
        assert!(floor >= -160.0 && ceil <= 20.0);
    }

    #[test]
    fn empty_frame_returns_none() {
        assert!(pick_levels(&[], -120.0, -20.0).is_none());
        assert!(pick_levels(&[10, 20], -50.0, -50.0).is_none());
    }
}

// ───────────────────────────── SSTV panel ──────────────────────────────

/// A transmit-image slot: the (bounded) source picture plus its thumbnail.
struct SstvSlot {
    src_rgb: Vec<u8>,
    sw: u16,
    sh: u16,
    tex: egui::TextureHandle,
}

/// A received-image gallery entry.
#[allow(dead_code)] // not used on wasm
struct SstvRecv {
    mode: Option<SstvMode>,
    tex: egui::TextureHandle,
}

/// SSTV panel state: received gallery, in-progress incoming image, transmit
/// slots, the overlay message, the current mode, and cached textures.
struct SstvUi {
    tx_mode: SstvMode,
    /// Operator callsign for the transmit-image header (mirrors the digi config).
    callsign: String,
    /// Auto mode: RX auto-detects the mode; TX defaults to Martin 1 until a mode
    /// is heard or the operator picks one.
    auto: bool,
    /// Overlay message per image slot (index-aligned with `slots`). The message
    /// box edits the entry for `selected_slot`, so switching slots swaps the
    /// text — and each is persisted alongside its picture.
    slot_messages: Vec<String>,
    slots: Vec<Option<SstvSlot>>,
    selected_slot: usize,
    received: Vec<SstvRecv>,
    /// In-progress incoming image (painted line-by-line).
    rx_color: Option<egui::ColorImage>,
    rx_tex: Option<egui::TextureHandle>,
    rx_id: u32,
    status: SstvStatus,
    /// Received-gallery index currently shown enlarged in an overlay window.
    enlarged: Option<usize>,
    /// Last VIS/free-run-detected mode we auto-applied to `tx_mode`, so a steady
    /// detection doesn't keep overriding the operator's manual mode choice.
    last_detected: Option<SstvMode>,
    preview_tex: Option<egui::TextureHandle>,
    preview_dirty: bool,
    loaded_disk: bool,
    /// File-picker result inbox (raw image bytes), filled by the picker task.
    inbox: Arc<Mutex<Option<Vec<u8>>>>,
    pick_target: Option<usize>,
}

impl Default for SstvUi {
    fn default() -> Self {
        SstvUi {
            tx_mode: SstvMode::Martin1,
            callsign: String::new(),
            auto: true,
            slot_messages: vec![String::new(); 5],
            slots: (0..5).map(|_| None).collect(),
            selected_slot: 0,
            received: Vec::new(),
            rx_color: None,
            rx_tex: None,
            rx_id: 0,
            status: SstvStatus::default(),
            enlarged: None,
            last_detected: None,
            preview_tex: None,
            preview_dirty: true,
            loaded_disk: false,
            inbox: Arc::new(Mutex::new(None)),
            pick_target: None,
        }
    }
}

impl SstvUi {
    /// A decoded scanline arrived: paint it into the in-progress image.
    fn on_line(&mut self, id: u32, y: u16, rgb: &[u8], ctx: &egui::Context) {
        let Some(mode) = self.status.detected else { return };
        let (w, h) = mode.dimensions();
        if self.rx_id != id || self.rx_color.is_none() {
            self.rx_id = id;
            self.rx_color = Some(crate::sstv::color_image(&vec![0u8; w as usize * h as usize * 3], w, h));
        }
        let Some(ci) = self.rx_color.as_mut() else { return };
        let (w, h) = (w as usize, h as usize);
        if (y as usize) < h && rgb.len() >= w * 3 {
            let row = y as usize * w;
            for x in 0..w {
                ci.pixels[row + x] = Color32::from_rgb(rgb[x * 3], rgb[x * 3 + 1], rgb[x * 3 + 2]);
            }
        }
        self.rx_tex =
            Some(ctx.load_texture("sstv_rx", ci.clone(), egui::TextureOptions::NEAREST));
    }

    /// A completed image arrived: decode and add it to the gallery.
    fn on_image(&mut self, _id: u32, mode: SstvMode, _w: u16, _h: u16, png: &[u8], ctx: &egui::Context) {
        if let Some((rgb, w, h)) = crate::sstv::decode_image(png) {
            let ci = crate::sstv::color_image(&rgb, w, h);
            let tex = ctx.load_texture("sstv_recv", ci, egui::TextureOptions::NEAREST);
            self.received.insert(0, SstvRecv { mode: Some(mode), tex });
            self.received.truncate(60);
        }
        self.rx_color = None;
        self.rx_tex = None;
    }

    /// The overlay message for the slot currently being edited.
    fn current_message(&self) -> &str {
        self.slot_messages.get(self.selected_slot).map(String::as_str).unwrap_or("")
    }

    /// Persist the per-slot overlay messages to the config file (native only).
    fn save_messages(&self) {
        sstv_save_messages(&self.slot_messages);
    }

    /// Rebuild the transmit preview when the mode, slot, or message changed.
    fn ensure_preview(&mut self, ctx: &egui::Context) {
        if !self.preview_dirty {
            return;
        }
        self.preview_dirty = false;
        let message = self.current_message().to_string();
        match self.slots.get(self.selected_slot).and_then(|s| s.as_ref()) {
            Some(slot) => {
                let (rgb, w, h) = crate::sstv::compose(
                    self.tx_mode,
                    &slot.src_rgb,
                    slot.sw,
                    slot.sh,
                    &message,
                    &self.callsign,
                );
                let ci = crate::sstv::color_image(&rgb, w, h);
                self.preview_tex =
                    Some(ctx.load_texture("sstv_preview", ci, egui::TextureOptions::NEAREST));
            }
            None => self.preview_tex = None,
        }
    }

    /// The composed PNG for the current selection, for transmit.
    fn compose_png(&self) -> Option<Vec<u8>> {
        let slot = self.slots.get(self.selected_slot).and_then(|s| s.as_ref())?;
        let (rgb, w, h) = crate::sstv::compose(
            self.tx_mode,
            &slot.src_rgb,
            slot.sw,
            slot.sh,
            self.current_message(),
            &self.callsign,
        );
        crate::sstv::encode_png(&rgb, w, h)
    }

    /// Accept a picked image file into `slot`, building a thumbnail texture.
    fn set_slot(&mut self, slot: usize, bytes: &[u8], ctx: &egui::Context) {
        let Some((rgb, w, h)) = crate::sstv::load_source_bounded(bytes, 1024) else { return };
        let ci = crate::sstv::color_image(&rgb, w, h);
        let tex = ctx.load_texture("sstv_slot", ci, egui::TextureOptions::LINEAR);
        if let Some(cell) = self.slots.get_mut(slot) {
            *cell = Some(SstvSlot { src_rgb: rgb, sw: w, sh: h, tex });
        }
        self.selected_slot = slot;
        self.preview_dirty = true;
        sstv_save_slot(slot, bytes);
    }
}

impl SdroxideApp {
    fn sstv_panel(&mut self, ui: &mut egui::Ui, cmds: &mut Vec<Command>) {
        let ctx = ui.ctx().clone();
        self.sstv_load_disk_once(&ctx);
        // Drain a completed file-pick (only consume the target once bytes arrive).
        let picked = self.sstv.inbox.lock().ok().and_then(|mut g| g.take());
        if let Some(bytes) = picked {
            if let Some(target) = self.sstv.pick_target.take() {
                self.sstv.set_slot(target, &bytes, &ctx);
            }
        }
        // Keep the header callsign in sync with the operator config.
        if self.sstv.callsign != self.digi_cfg_edit.my_call {
            self.sstv.callsign = self.digi_cfg_edit.my_call.clone();
            self.sstv.preview_dirty = true;
        }
        self.sstv.ensure_preview(&ctx);
        ctx.request_repaint_after(Duration::from_millis(120));

        let st = self.sstv.status;
        let tx_active = st.tx_active;

        // Whole-panel size. The mode/signal/slant controls sit in a boxed strip
        // on the left above LIVE + RECEIVED; the transmit compositor spans the
        // full height on the right, reclaiming the space the old full-width
        // control rows used to leave empty at the top.
        let avail = ui.available_size();
        let full_h = avail.y;
        let handle_w = 7.0;
        // TRANSMIT (send) column takes a user-draggable fraction of the width; the
        // receive side (LIVE + RECEIVED) gets the rest. Each keeps a usable minimum.
        let tx_w = (avail.x * self.view.sstv_tx_fraction)
            .clamp(300.0, (avail.x - handle_w - 300.0).max(300.0));
        let left_w = (avail.x - tx_w - handle_w).max(300.0);
        // LIVE takes the rest of the receive side; the RECEIVED gallery width is a
        // user-draggable fraction of it (min one thumbnail column).
        let gallery_w = (left_w * self.view.sstv_gallery_fraction)
            .clamp(150.0, (left_w - handle_w - 160.0).max(150.0));
        let live_w = (left_w - gallery_w - handle_w).max(160.0);

        ui.horizontal_top(|ui| {
            // A received thumbnail was clicked → enlarge it (applied after the row).
            let mut enlarge: Option<usize> = None;

            // ── LEFT: boxed controls, then LIVE + RECEIVED ──
            ui.allocate_ui_with_layout(
                egui::vec2(left_w, full_h),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    egui::Frame::new()
                        .fill(crate::theme::ROW_BG)
                        .stroke(egui::Stroke::new(1.0, crate::theme::LINE_LIT))
                        .inner_margin(egui::Margin { left: 8, right: 8, top: 6, bottom: 7 })
                        .show(ui, |ui| {
                            ui.set_min_width(left_w - 16.0);
                            ui.set_max_width(left_w - 16.0);

                            // Mode selection: Auto + the per-mode chips.
                            ui.horizontal_wrapped(|ui| {
                                ui.label(
                                    RichText::new("SSTV")
                                        .size(12.0)
                                        .strong()
                                        .color(crate::theme::CYAN),
                                );
                                let auto_label = if self.sstv.auto {
                                    format!("Auto ({})", self.sstv.tx_mode.label())
                                } else {
                                    "Auto".to_string()
                                };
                                if crate::chrome::chip(ui, self.sstv.auto, &auto_label).clicked() {
                                    self.sstv.auto = true;
                                    self.sstv.tx_mode = SstvMode::Martin1;
                                    self.sstv.preview_dirty = true;
                                    cmds.push(Command::SstvSetMode(None));
                                }
                                for m in SstvMode::ALL {
                                    let active = !self.sstv.auto && self.sstv.tx_mode == m;
                                    if crate::chrome::chip(ui, active, m.label()).clicked() {
                                        self.sstv.auto = false;
                                        self.sstv.tx_mode = m;
                                        self.sstv.preview_dirty = true;
                                        cmds.push(Command::SstvSetMode(Some(m)));
                                    }
                                }
                            });
                            ui.add_space(5.0);

                            // Signal meter + activity, and the TX-slant trim.
                            ui.horizontal_wrapped(|ui| {
                                ui.label(RichText::new("Signal").size(10.0).weak());
                                sstv_level_bar(ui, st.signal);
                                if tx_active {
                                    ui.label(
                                        RichText::new(format!("● TX {:.0}%", st.progress * 100.0))
                                            .size(11.0)
                                            .strong()
                                            .color(crate::theme::PINK),
                                    );
                                } else if st.rx_active {
                                    ui.label(
                                        RichText::new(format!("● RX {:.0}%", st.progress * 100.0))
                                            .size(11.0)
                                            .strong()
                                            .color(crate::theme::GREEN),
                                    );
                                } else if let Some(m) = st.detected {
                                    ui.label(
                                        RichText::new(format!("last: {}", m.label()))
                                            .size(10.0)
                                            .weak(),
                                    );
                                } else {
                                    ui.label(RichText::new("listening…").size(10.0).weak());
                                }

                                ui.add_space(12.0);
                                ui.separator();
                                ui.label(RichText::new("TX slant").size(10.0).weak()).on_hover_text(
                                    "Transmit clock trim (ppm) to remove slant on the far-end decoder",
                                );
                                ui.add_enabled_ui(self.digi_cfg_seeded, |ui| {
                                    ui.spacing_mut().slider_width = 130.0;
                                    let resp = ui.add(
                                        egui::Slider::new(
                                            &mut self.digi_cfg_edit.sstv_tx_ppm,
                                            -5000.0..=5000.0,
                                        )
                                        .suffix(" ppm")
                                        .fixed_decimals(0),
                                    );
                                    if resp.drag_stopped() || (resp.changed() && !resp.dragged()) {
                                        cmds.push(Command::SetDigiConfig(self.digi_cfg_edit.clone()));
                                    }
                                    if ui
                                        .small_button("0")
                                        .on_hover_text("Reset to 0 ppm")
                                        .clicked()
                                    {
                                        self.digi_cfg_edit.sstv_tx_ppm = 0.0;
                                        cmds.push(Command::SetDigiConfig(self.digi_cfg_edit.clone()));
                                    }
                                });
                            });
                        });
                    ui.add_space(6.0);

                    // LIVE + RECEIVED fill the remaining height of the left column.
                    let row_h = ui.available_height().max(160.0);
                    ui.horizontal_top(|ui| {
                        // LIVE: the picture currently decoding, shown large.
                        sstv_section(ui, "LIVE", egui::vec2(live_w, row_h), |ui| {
                            ui.centered_and_justified(|ui| {
                                if let Some(tex) = &self.sstv.rx_tex {
                                    ui.add(
                                        egui::Image::new(tex)
                                            .max_height(row_h - 34.0)
                                            .max_width(live_w - 16.0),
                                    );
                                } else {
                                    let msg = if st.signal > 0.0008 {
                                        "waiting for a signal…"
                                    } else {
                                        "no / low audio"
                                    };
                                    ui.label(RichText::new(msg).size(11.0).weak());
                                }
                            });
                        });
                        // Draggable vertical divider between LIVE and RECEIVED.
                        let (hrect, hresp) = ui.allocate_exact_size(
                            egui::vec2(handle_w, row_h),
                            egui::Sense::click_and_drag(),
                        );
                        if hresp.hovered() || hresp.dragged() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                        }
                        if hresp.dragged() {
                            // Dragging right shrinks the gallery (grows LIVE).
                            let d = hresp.drag_delta().x / left_w.max(1.0);
                            self.view.sstv_gallery_fraction =
                                (self.view.sstv_gallery_fraction - d).clamp(0.2, 0.6);
                        }
                        {
                            let p = ui.painter_at(hrect);
                            let hot = hresp.hovered() || hresp.dragged();
                            let col = if hot { crate::theme::CYAN } else { Color32::from_gray(70) };
                            let (cx, cy) = (hrect.center().x, hrect.center().y);
                            for dy in [-16.0f32, 0.0, 16.0] {
                                p.line_segment(
                                    [egui::pos2(cx, cy + dy - 6.0), egui::pos2(cx, cy + dy + 6.0)],
                                    egui::Stroke::new(2.0, col),
                                );
                            }
                        }

                        // RECEIVED: narrow multi-column gallery of decoded pictures.
                        sstv_section(ui, "RECEIVED", egui::vec2(gallery_w, row_h), |ui| {
                            if self.sstv.received.is_empty() {
                                ui.label(
                                    RichText::new("Decoded pictures collect here.")
                                        .size(11.0)
                                        .weak(),
                                );
                                return;
                            }
                            let thumb = egui::vec2(112.0, 90.0);
                            egui::ScrollArea::vertical()
                                .id_salt("sstv-gallery")
                                .max_height(row_h - 24.0)
                                .auto_shrink([false, false])
                                .show_themed(ui, |ui| {
                                    ui.horizontal_wrapped(|ui| {
                                        ui.spacing_mut().item_spacing = egui::vec2(5.0, 5.0);
                                        for (i, r) in self.sstv.received.iter().enumerate() {
                                            let resp = ui
                                                .add(
                                                    egui::Image::new(&r.tex)
                                                        .fit_to_exact_size(thumb)
                                                        .corner_radius(2.0)
                                                        .sense(egui::Sense::click()),
                                                )
                                                .on_hover_text("Click to enlarge");
                                            if resp.clicked() {
                                                enlarge = Some(i);
                                            }
                                        }
                                    });
                                });
                        });
                    });
                },
            );

            // Draggable vertical divider between the receive side and the
            // TRANSMIT (send) column — mirrors the FT8 decode/QSO splitter.
            let (hrect, hresp) =
                ui.allocate_exact_size(egui::vec2(handle_w, full_h), egui::Sense::click_and_drag());
            if hresp.hovered() || hresp.dragged() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
            }
            if hresp.dragged() {
                // Dragging right shrinks the TX column (grows the receive side).
                let d = hresp.drag_delta().x / avail.x.max(1.0);
                self.view.sstv_tx_fraction = (self.view.sstv_tx_fraction - d).clamp(0.22, 0.6);
            }
            {
                let p = ui.painter_at(hrect);
                let hot = hresp.hovered() || hresp.dragged();
                let col = if hot { crate::theme::CYAN } else { Color32::from_gray(70) };
                let (cx, cy) = (hrect.center().x, hrect.center().y);
                for dy in [-16.0f32, 0.0, 16.0] {
                    p.line_segment(
                        [egui::pos2(cx, cy + dy - 6.0), egui::pos2(cx, cy + dy + 6.0)],
                        egui::Stroke::new(2.0, col),
                    );
                }
            }

            // ── RIGHT: transmit compositor, full height ──
            ui.allocate_ui(egui::vec2(tx_w, full_h), |ui| {
                sstv_section(ui, "TRANSMIT", egui::vec2(tx_w, full_h), |ui| {
                    let inner_w = tx_w - 16.0;

                    // Five source slots — the highlighted one acts as the active
                    // "tab" whose message the box below edits.
                    ui.label(
                        RichText::new("Image slots — click one to edit its message")
                            .size(9.5)
                            .weak(),
                    );
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 5.0;
                        for i in 0..self.sstv.slots.len() {
                            let sel = self.sstv.selected_slot == i;
                            let size = egui::vec2(70.0, 54.0);
                            let resp = if let Some(slot) = &self.sstv.slots[i] {
                                ui.add(
                                    egui::Image::new(&slot.tex)
                                        .fit_to_exact_size(size)
                                        .corner_radius(2.0)
                                        .sense(egui::Sense::click()),
                                )
                            } else {
                                let (rect, resp) =
                                    ui.allocate_exact_size(size, egui::Sense::click());
                                ui.painter().rect_stroke(
                                    rect,
                                    2.0,
                                    egui::Stroke::new(1.0, Color32::from_gray(70)),
                                    egui::StrokeKind::Inside,
                                );
                                ui.painter().text(
                                    rect.center(),
                                    egui::Align2::CENTER_CENTER,
                                    "+",
                                    egui::FontId::proportional(22.0),
                                    Color32::from_gray(110),
                                );
                                resp
                            };
                            // Active-tab highlight: a cyan wash + heavier border so
                            // it is obvious which slot the message box targets.
                            if sel {
                                ui.painter().rect_filled(
                                    resp.rect,
                                    2.0,
                                    Color32::from_rgba_unmultiplied(0x00, 0xd0, 0xf4, 34),
                                );
                                ui.painter().rect_stroke(
                                    resp.rect,
                                    2.0,
                                    egui::Stroke::new(2.5, crate::theme::CYAN),
                                    egui::StrokeKind::Outside,
                                );
                            }
                            // Slot number badge (1..5), like a tab label.
                            let badge = egui::Rect::from_min_size(
                                resp.rect.left_top() + egui::vec2(2.0, 2.0),
                                egui::vec2(15.0, 13.0),
                            );
                            ui.painter().rect_filled(badge, 2.0, Color32::from_black_alpha(150));
                            ui.painter().text(
                                badge.center(),
                                egui::Align2::CENTER_CENTER,
                                format!("{}", i + 1),
                                egui::FontId::proportional(10.0),
                                if sel { crate::theme::CYAN } else { Color32::from_gray(170) },
                            );
                            let resp = resp.on_hover_text(
                                "Click to edit this slot's message · double-click to load an image",
                            );
                            if resp.double_clicked() {
                                self.sstv.pick_target = Some(i);
                                pick_image(self.sstv.inbox.clone());
                            } else if resp.clicked() && !sel {
                                self.sstv.save_messages(); // flush the slot we leave
                                self.sstv.selected_slot = i;
                                self.sstv.preview_dirty = true;
                            }
                        }
                    });
                    ui.add_space(5.0);

                    // Explicit image load button for the active slot.
                    ui.horizontal(|ui| {
                        let sel = self.sstv.selected_slot;
                        let has_img =
                            self.sstv.slots.get(sel).map(|s| s.is_some()) == Some(true);
                        let label = if has_img { "Change image…" } else { "Load image…" };
                        if crate::chrome::chip(ui, false, label).clicked() {
                            self.sstv.pick_target = Some(sel);
                            pick_image(self.sstv.inbox.clone());
                        }
                    });
                    ui.add_space(6.0);

                    // Preview gets a capped share of the height; the message box
                    // grows to fill whatever's left above the buttons.
                    let btn_h = 42.0;
                    let gap = 6.0;
                    ui.label(RichText::new("Preview (what is transmitted)").size(9.5).weak());
                    let preview_h = (ui.available_height() * 0.45).clamp(80.0, 260.0);
                    egui::Frame::new()
                        .fill(Color32::from_gray(6))
                        .stroke(egui::Stroke::new(1.0, crate::theme::LINE_LIT))
                        .inner_margin(2.0)
                        .show(ui, |ui| {
                            ui.set_min_size(egui::vec2(inner_w, preview_h));
                            ui.set_max_size(egui::vec2(inner_w, preview_h));
                            ui.centered_and_justified(|ui| {
                                if let Some(tex) = &self.sstv.preview_tex {
                                    ui.add(
                                        egui::Image::new(tex)
                                            .max_height(preview_h - 4.0)
                                            .max_width(inner_w - 4.0),
                                    );
                                } else {
                                    ui.label(
                                        RichText::new("Load an image into this slot →")
                                            .size(11.0)
                                            .weak(),
                                    );
                                }
                            });
                        });
                    ui.add_space(gap);

                    // Overlay message for the active slot — fills the height above
                    // the buttons; persisted when focus leaves the box or the slot
                    // changes. A per-slot id keeps each tab's cursor independent.
                    let sel = self.sstv.selected_slot;
                    let msg_h = (ui.available_height() - btn_h - gap).max(48.0);
                    let resp = ui
                        .push_id(sel, |ui| {
                            ui.add_sized(
                                egui::vec2(inner_w, msg_h),
                                egui::TextEdit::multiline(&mut self.sstv.slot_messages[sel])
                                    .hint_text("Drawn on this slot's image"),
                            )
                        })
                        .inner;
                    if resp.changed() {
                        self.sstv.preview_dirty = true;
                    }
                    if resp.lost_focus() {
                        self.sstv.save_messages();
                    }
                    ui.add_space(gap);

                    // Large cut-corner TX / ABORT buttons.
                    ui.horizontal(|ui| {
                        let can_tx = self.sstv.slots.get(self.sstv.selected_slot).map(|s| s.is_some())
                            == Some(true)
                            && !tx_active;
                        let tx = ui
                            .add_enabled_ui(can_tx, |ui| {
                                crate::chrome::chip_accent(
                                    ui,
                                    can_tx,
                                    RichText::new("   TX   ").size(16.0).strong(),
                                    crate::theme::PINK,
                                    Color32::WHITE,
                                )
                            })
                            .inner;
                        if tx.clicked() {
                            self.sstv.save_messages(); // capture any unfocused edit
                            if let Some(png) = self.sstv.compose_png() {
                                cmds.push(Command::SstvTx { mode: self.sstv.tx_mode, png });
                            }
                        }
                        ui.add_space(8.0);
                        let abort = ui
                            .add_enabled_ui(tx_active, |ui| {
                                crate::chrome::chip(
                                    ui,
                                    false,
                                    RichText::new(" ABORT TX ").size(15.0).strong(),
                                )
                            })
                            .inner;
                        if abort.clicked() {
                            cmds.push(Command::DigiAbortTx);
                        }
                    });
                });
            });

            if let Some(i) = enlarge {
                self.sstv.enlarged = Some(i);
            }
        });

        // Enlarged view of a clicked received image (overlay window).
        if let Some(idx) = self.sstv.enlarged {
            let mut open = true;
            if let Some(r) = self.sstv.received.get(idx) {
                egui::Window::new("Received image")
                    .open(&mut open)
                    .collapsible(false)
                    .resizable(true)
                    .default_size([660.0, 528.0])
                    .frame(crate::chrome::window_frame())
                    .show(&ctx, |ui| {
                        // Scale up to fill the window width (preserving aspect).
                        let native = r.tex.size_vec2();
                        let avail_w = ui.available_width().min(1000.0);
                        let scale = (avail_w / native.x.max(1.0)).clamp(1.0, 4.0);
                        ui.add(egui::Image::new(&r.tex).fit_to_exact_size(native * scale));
                    });
            } else {
                open = false;
            }
            if !open {
                self.sstv.enlarged = None;
            }
        }
    }

    /// On first entry, load any persisted transmit slots and received gallery
    /// from disk (native only).
    fn sstv_load_disk_once(&mut self, ctx: &egui::Context) {
        if self.sstv.loaded_disk {
            return;
        }
        self.sstv.loaded_disk = true;
        for (i, entry) in sstv_load_slots().into_iter().enumerate() {
            if let Some((rgb, w, h)) = entry {
                let ci = crate::sstv::color_image(&rgb, w, h);
                let tex = ctx.load_texture("sstv_slot", ci, egui::TextureOptions::LINEAR);
                if let Some(cell) = self.sstv.slots.get_mut(i) {
                    *cell = Some(SstvSlot { src_rgb: rgb, sw: w, sh: h, tex });
                }
            }
        }
        // Restore the per-slot overlay messages (padded to the slot count).
        for (i, msg) in sstv_load_messages().into_iter().enumerate() {
            if let Some(cell) = self.sstv.slot_messages.get_mut(i) {
                *cell = msg;
            }
        }
        for (rgb, w, h) in sstv_load_gallery() {
            let ci = crate::sstv::color_image(&rgb, w, h);
            let tex = ctx.load_texture("sstv_recv", ci, egui::TextureOptions::NEAREST);
            self.sstv.received.push(SstvRecv { mode: None, tex });
        }
    }
}

// ───────────────────────── RF Paint panel ──────────────────────────────

/// RF Paint (Spectrum Painting) panel state: the text to paint, a loaded image
/// (as a bounded grayscale bitmap), and the colour-mapped preview textures shown
/// in the two scrolling "preview waterfall" boxes.
struct RfPaintUi {
    /// The line of text to paint.
    text: String,
    /// The text the current preview texture was built from (rebuild on change).
    text_built_for: String,
    /// Grayscale, vertically-tiling preview of the text banner.
    text_prev: Option<egui::TextureHandle>,
    /// Loaded source image as a grayscale bitmap (bounded to the paint size).
    img_gray: Option<(Vec<u8>, u16, u16)>,
    /// Grayscale display of the loaded image (for the image box).
    img_disp: Option<egui::TextureHandle>,
    /// Grayscale, vertically-tiling preview of the image.
    img_prev: Option<egui::TextureHandle>,
    img_dirty: bool,
    /// File-picker result inbox (raw image bytes), filled by the picker task.
    inbox: Arc<Mutex<Option<Vec<u8>>>>,
}

impl Default for RfPaintUi {
    fn default() -> Self {
        RfPaintUi {
            text: String::new(),
            text_built_for: "\0".to_string(), // sentinel != "" so an empty box builds once
            text_prev: None,
            img_gray: None,
            img_disp: None,
            img_prev: None,
            img_dirty: true,
            inbox: Arc::new(Mutex::new(None)),
        }
    }
}

impl RfPaintUi {
    /// Rebuild the preview textures whose source (text or image) changed since
    /// the last frame.
    fn ensure(&mut self, ctx: &egui::Context) {
        // Text banner preview.
        if self.text_built_for != self.text {
            self.text_built_for = self.text.clone();
            match crate::rf_paint::text_bitmap(&self.text) {
                Some((gray, w, h)) => {
                    let ci = crate::rf_paint::preview_gray_image(&gray, w, h);
                    self.text_prev = Some(load_scroll_tex(ctx, "rfpaint_text", ci));
                }
                None => {
                    self.text_prev = None;
                }
            }
        }
        // Image preview + grayscale display.
        if self.img_dirty {
            self.img_dirty = false;
            match &self.img_gray {
                Some((gray, w, h)) => {
                    let ci = crate::rf_paint::preview_gray_image(gray, *w, *h);
                    self.img_prev = Some(load_scroll_tex(ctx, "rfpaint_img_prev", ci));
                    // Natural grayscale for the image box.
                    let (iw, ih) = (*w as usize, *h as usize);
                    let mut disp =
                        egui::ColorImage::new([iw, ih], vec![Color32::BLACK; iw * ih]);
                    for (px, &v) in disp.pixels.iter_mut().zip(gray.iter()) {
                        *px = Color32::from_gray(v);
                    }
                    self.img_disp =
                        Some(ctx.load_texture("rfpaint_img_disp", disp, egui::TextureOptions::LINEAR));
                }
                None => {
                    self.img_prev = None;
                    self.img_disp = None;
                }
            }
        }
    }
}

/// Load a preview texture that tiles vertically (so the scrolling waterfall loops
/// seamlessly) with crisp pixel edges.
fn load_scroll_tex(ctx: &egui::Context, name: &str, ci: egui::ColorImage) -> egui::TextureHandle {
    let mut opt = egui::TextureOptions::NEAREST;
    opt.wrap_mode = egui::TextureWrapMode::Repeat;
    ctx.load_texture(name, ci, opt)
}

/// Draw a scrolling "preview waterfall" of `tex` in a fixed box: the picture
/// (frequency across the width) slides downward like a live waterfall.
fn draw_scroll_preview(
    ui: &mut egui::Ui,
    tex: Option<&egui::TextureHandle>,
    size: egui::Vec2,
    time: f64,
    empty_hint: &str,
) {
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 2.0, Color32::from_gray(8));
    match tex {
        Some(tex) => {
            // Frequency (texture width) fills the box width; time (texture height)
            // scrolls through a window sized so the pixels stay roughly square, so
            // a long banner scrolls by at a readable scale instead of squashing to
            // fit. Newest data enters at the top and flows down (the top of the
            // picture renders first), like a standard waterfall; the scroll speed
            // is a constant rows/sec.
            let [tw, th] = tex.size();
            let (tw, th) = (tw.max(1) as f32, th.max(1) as f32);
            let px_per_bin = rect.width() / tw;
            let rows_visible = (rect.height() / px_per_bin).max(1.0);
            let vspan = (rows_visible / th).min(1.0);
            // Decreasing offset scrolls the sampled window up in texture space, so
            // content moves down on screen (newest at top).
            let off = (-(time as f32) * 45.0 / th).rem_euclid(1.0);
            let uv = egui::Rect::from_min_max(egui::pos2(0.0, off), egui::pos2(1.0, off + vspan));
            egui::Image::new(tex).uv(uv).paint_at(ui, rect);
            ui.ctx().request_repaint(); // keep the scroll animating
        }
        None => {
            p.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                empty_hint,
                egui::FontId::proportional(11.0),
                Color32::from_gray(90),
            );
        }
    }
    p.rect_stroke(
        rect,
        2.0,
        egui::Stroke::new(1.0, crate::theme::LINE_LIT),
        egui::StrokeKind::Inside,
    );
}

/// Draw a static, aspect-preserved image box (the loaded picture, or a hint).
fn draw_image_box(
    ui: &mut egui::Ui,
    tex: Option<&egui::TextureHandle>,
    dims: Option<(u16, u16)>,
    size: egui::Vec2,
    empty_hint: &str,
) {
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 2.0, Color32::from_gray(10));
    if let (Some(tex), Some((w, h))) = (tex, dims) {
        let inner = rect.shrink(3.0);
        let ar = w as f32 / h.max(1) as f32;
        let (mut fw, mut fh) = (inner.width(), inner.width() / ar);
        if fh > inner.height() {
            fh = inner.height();
            fw = inner.height() * ar;
        }
        let fr = egui::Rect::from_center_size(inner.center(), egui::vec2(fw, fh));
        egui::Image::new(tex).paint_at(ui, fr);
    } else {
        p.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            empty_hint,
            egui::FontId::proportional(11.0),
            Color32::from_gray(90),
        );
    }
    p.rect_stroke(
        rect,
        2.0,
        egui::Stroke::new(1.0, crate::theme::LINE_LIT),
        egui::StrokeKind::Inside,
    );
}

impl SdroxideApp {
    /// The RF Paint (Spectrum Painting) operating panel: a text-paint area and an
    /// image-paint area side by side, each with a scrolling preview waterfall and
    /// a transmit button. Transmit rides the ordinary image-transmit command
    /// (`DigiImageTx`); the `RfPaintController` turns the bitmap into tones.
    /// FreeDV RADE V1 digital voice.
    ///
    /// There is no text or image to show and no tone offset to tune: the whole
    /// operating surface is "am I locked to the far end, how good is the link,
    /// and am I talking".
    fn rade_panel(&mut self, ui: &mut egui::Ui, cmds: &mut Vec<Command>, _panel_h: f32) {
        let status = self.digi_status.clone();
        let rade = status.as_ref().and_then(|s| s.rade).unwrap_or_default();
        let transmitting = status.as_ref().map(|s| s.transmitting).unwrap_or(false);

        ui.horizontal(|ui| {
            ui.label(RichText::new("RADE").size(11.0).strong().color(crate::theme::CYAN));
            ui.label(
                RichText::new("FreeDV V1 digital voice")
                    .size(10.5)
                    .color(crate::theme::CYAN_DIM),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if transmitting {
                    ui.label(RichText::new("● TX").size(11.0).strong().color(crate::theme::PINK));
                    ui.add_space(8.0);
                }
                // Silence the raw signal, leaving only decoded speech audible.
                let muted = self.digi_cfg_edit.rade_mute_analog;
                let resp =
                    crate::chrome::chip(ui, muted, RichText::new("MUTE ANALOG").size(10.5));
                if resp.clicked() && self.digi_cfg_seeded {
                    self.digi_cfg_edit.rade_mute_analog = !muted;
                    cmds.push(Command::SetDigiConfig(self.digi_cfg_edit.clone()));
                }
                resp.on_hover_text(
                    "Mute the demodulated audio, so only decoded speech is heard. \
                     The raw signal is otherwise passed through whenever the modem \
                     has nothing to play — that hiss is how you find an over before \
                     it syncs, so leave this off while tuning.",
                );
            });
        });
        ui.add_space(8.0);

        // Sync lamp + link readouts.
        ui.horizontal(|ui| {
            let (lamp, _) =
                ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
            let lit = rade.sync && !transmitting;
            ui.painter_at(lamp).circle_filled(
                lamp.center(),
                5.5,
                if lit { crate::theme::GREEN } else { Color32::from_gray(48) },
            );
            ui.label(
                RichText::new(if transmitting {
                    "transmitting"
                } else if rade.sync {
                    "SYNC"
                } else {
                    "searching"
                })
                .size(12.0)
                .strong()
                .color(if lit { crate::theme::GREEN } else { Color32::from_gray(130) }),
            );
            ui.add_space(16.0);
            let dim = Color32::from_gray(150);
            if rade.sync && !transmitting {
                ui.label(
                    RichText::new(format!("SNR {:.0} dB", rade.snr_db))
                        .size(12.0)
                        .color(crate::theme::TEXT_STRONG),
                );
                ui.add_space(12.0);
                ui.label(
                    RichText::new(format!("offset {:+.0} Hz", rade.freq_offset_hz))
                        .size(11.0)
                        .color(dim),
                )
                .on_hover_text(
                    "How far the received signal sits from where the modem expects it. \
                     Large values still decode — the acquisition loop tracks them — but \
                     nudging the dial to bring this near zero gives the best margin.",
                );
            } else {
                ui.label(RichText::new("SNR —").size(12.0).color(dim));
            }
        });
        ui.add_space(8.0);

        // Decoded-speech level.
        {
            let (bar, _) =
                ui.allocate_exact_size(egui::vec2(ui.available_width(), 6.0), egui::Sense::hover());
            let p = ui.painter_at(bar);
            p.rect_filled(bar, 0.0, Color32::from_gray(22));
            let level = rade.rx_level.clamp(0.0, 1.0);
            if level > 0.0 && !transmitting {
                let mut fill = bar;
                fill.set_width(bar.width() * level);
                p.rect_filled(fill, 0.0, crate::theme::CYAN);
            }
        }
        ui.add_space(12.0);

        // Transmit. `DigiTxActive` is the same command the main PTT button ends
        // up sending in this mode, so the two stay in step.
        ui.horizontal(|ui| {
            let label = if transmitting { "STOP TALKING" } else { "TALK (PTT)" };
            let resp = crate::chrome::chip_accent(
                ui,
                transmitting,
                RichText::new(label).size(13.0).strong(),
                crate::theme::PINK,
                crate::theme::TEXT_STRONG,
            );
            if resp.clicked() {
                cmds.push(Command::DigiTxActive(!transmitting));
            }
            resp.on_hover_text(
                "Open or close a RADE over. The modem needs ~120 ms of speech before \
                 the first frame goes out, and sends an end-of-over frame when you stop, \
                 so transmit runs on a little past the button.",
            );
            ui.add_space(10.0);
        });
        ui.add_space(12.0);

        ui.label(
            RichText::new(
                "RADE V1 occupies roughly 1060–1880 Hz of the USB passband. Put the \
                 signal inside the shaded band on the waterfall; the modem finds it from \
                 there.",
            )
            .size(10.5)
            .color(Color32::from_gray(125)),
        );
        if rade.dropped > 0 {
            ui.add_space(6.0);
            ui.label(
                RichText::new(format!(
                    "⚠ {} samples dropped between the receiver and the decoder — this \
                     machine is not keeping up with the neural decode.",
                    rade.dropped
                ))
                .size(10.5)
                .color(crate::theme::YELLOW),
            );
        }
    }

    fn rf_paint_panel(&mut self, ui: &mut egui::Ui, cmds: &mut Vec<Command>, _panel_h: f32) {
        let ctx = ui.ctx().clone();
        let time = ctx.input(|i| i.time);

        // Absorb a freshly-picked image file.
        if let Some(bytes) = self.rf_paint.inbox.lock().ok().and_then(|mut g| g.take()) {
            self.rf_paint.img_gray = crate::rf_paint::image_bitmap(&bytes);
            self.rf_paint.img_dirty = true;
        }
        self.rf_paint.ensure(&ctx);

        let status = self.digi_status.clone();
        let transmitting = status.as_ref().map(|s| s.transmitting).unwrap_or(false);
        let progress =
            status.as_ref().map(|s| (s.tx_sent as f32 / 1000.0).clamp(0.0, 1.0)).unwrap_or(0.0);

        // Header: title, transmit-speed slider, and the transmit indicator.
        ui.horizontal(|ui| {
            ui.label(RichText::new("RF PAINT").size(11.0).strong().color(crate::theme::CYAN));
            ui.add_space(12.0);
            ui.label(RichText::new("Scan speed").size(10.5).color(crate::theme::CYAN_DIM));
            let mut speed = self.digi_cfg_edit.rf_paint_speed;
            ui.spacing_mut().slider_width = 150.0;
            let resp = ui
                .add(
                    egui::Slider::new(&mut speed, 0.0625..=1.0)
                        .logarithmic(true)
                        .custom_formatter(|v, _| format!("{:.0}%", v * 100.0))
                        .custom_parser(|s| {
                            s.trim().trim_end_matches('%').parse::<f64>().ok().map(|p| p / 100.0)
                        }),
                )
                .on_hover_text(
                    "How fast the text/image is scanned onto the waterfall. Lower is slower and \
                     more legible; 100% = base rate, 25% (centre) is the default.",
                );
            if resp.changed() {
                self.digi_cfg_edit.rf_paint_speed = speed;
                cmds.push(Command::SetDigiConfig(self.digi_cfg_edit.clone()));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if transmitting {
                    if crate::chrome::chip(ui, false, "Abort").clicked() {
                        cmds.push(Command::DigiAbortTx);
                    }
                    ui.label(
                        RichText::new(format!("● TX {:.0}%", progress * 100.0))
                            .size(11.0)
                            .strong()
                            .color(crate::theme::PINK),
                    );
                }
            });
        });
        // Transmit-progress bar spanning the panel while keyed.
        {
            let (bar, _) =
                ui.allocate_exact_size(egui::vec2(ui.available_width(), 3.0), egui::Sense::hover());
            let p = ui.painter_at(bar);
            p.rect_filled(bar, 0.0, Color32::from_gray(24));
            if transmitting {
                let mut fill = bar;
                fill.set_width(bar.width() * progress);
                p.rect_filled(fill, 0.0, crate::theme::PINK);
            }
        }
        ui.add_space(4.0);

        let avail_w = ui.available_width();
        let gap = 10.0;
        let half = ((avail_w - gap) / 2.0).max(150.0);
        let content_h = (ui.available_height() - 2.0).max(150.0);

        ui.horizontal_top(|ui| {
            // ── Text paint ──
            sstv_section(ui, "TEXT PAINT", egui::vec2(half, content_h), |ui| {
                let inner_w = ui.available_width();
                ui.add(
                    egui::TextEdit::singleline(&mut self.rf_paint.text)
                        .hint_text("Type text to paint…")
                        .desired_width(inner_w),
                );
                ui.add_space(6.0);
                ui.label(
                    RichText::new("PREVIEW WATERFALL").size(8.5).color(crate::theme::CYAN_DIM),
                );
                ui.add_space(2.0);
                let prev_h = (ui.available_height() - 40.0).max(44.0);
                draw_scroll_preview(
                    ui,
                    self.rf_paint.text_prev.as_ref(),
                    egui::vec2(inner_w, prev_h),
                    time,
                    "type text to preview",
                );
                ui.add_space(6.0);
                let ready = !self.rf_paint.text.trim().is_empty();
                ui.add_enabled_ui(ready && !transmitting, |ui| {
                    if crate::chrome::chip_accent(
                        ui,
                        true,
                        "  TRANSMIT  ",
                        crate::theme::PINK,
                        Color32::WHITE,
                    )
                    .clicked()
                        && let Some((gray, w, h)) =
                            crate::rf_paint::text_bitmap(&self.rf_paint.text)
                        && let Some(png) = crate::rf_paint::gray_to_png(&gray, w, h)
                    {
                        cmds.push(Command::DigiImageTx { png });
                    }
                });
            });
            ui.add_space(gap);
            // ── Image paint ──
            sstv_section(ui, "IMAGE PAINT", egui::vec2(half, content_h), |ui| {
                let inner_w = ui.available_width();
                let img_h = (content_h * 0.4).clamp(56.0, 150.0);
                draw_image_box(
                    ui,
                    self.rf_paint.img_disp.as_ref(),
                    self.rf_paint.img_gray.as_ref().map(|(_, w, h)| (*w, *h)),
                    egui::vec2(inner_w, img_h),
                    "no image loaded",
                );
                ui.add_space(4.0);
                if crate::chrome::chip(ui, false, "Load image…").clicked() {
                    pick_image(self.rf_paint.inbox.clone());
                }
                ui.add_space(6.0);
                ui.label(
                    RichText::new("PREVIEW WATERFALL").size(8.5).color(crate::theme::CYAN_DIM),
                );
                ui.add_space(2.0);
                let prev_h = (ui.available_height() - 40.0).max(40.0);
                draw_scroll_preview(
                    ui,
                    self.rf_paint.img_prev.as_ref(),
                    egui::vec2(inner_w, prev_h),
                    time,
                    "load an image to preview",
                );
                ui.add_space(6.0);
                let ready = self.rf_paint.img_gray.is_some();
                ui.add_enabled_ui(ready && !transmitting, |ui| {
                    if crate::chrome::chip_accent(
                        ui,
                        true,
                        "  TRANSMIT  ",
                        crate::theme::PINK,
                        Color32::WHITE,
                    )
                    .clicked()
                        && let Some((gray, w, h)) = &self.rf_paint.img_gray
                        && let Some(png) = crate::rf_paint::gray_to_png(gray, *w, *h)
                    {
                        cmds.push(Command::DigiImageTx { png });
                    }
                });
            });
        });
    }
}

// ── File picker (native thread / wasm async) ──

#[cfg(not(target_arch = "wasm32"))]
fn pick_image(inbox: Arc<Mutex<Option<Vec<u8>>>>) {
    std::thread::spawn(move || {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("Image", &["png", "jpg", "jpeg"])
            .pick_file()
        {
            if let Ok(bytes) = std::fs::read(&path) {
                if let Ok(mut g) = inbox.lock() {
                    *g = Some(bytes);
                }
            }
        }
    });
}

#[cfg(target_arch = "wasm32")]
fn pick_image(inbox: Arc<Mutex<Option<Vec<u8>>>>) {
    wasm_bindgen_futures::spawn_local(async move {
        if let Some(file) = rfd::AsyncFileDialog::new()
            .add_filter("Image", &["png", "jpg", "jpeg"])
            .pick_file()
            .await
        {
            let bytes = file.read().await;
            if let Ok(mut g) = inbox.lock() {
                *g = Some(bytes);
            }
        }
    });
}

// ── Disk persistence (native only) ──

#[cfg(not(target_arch = "wasm32"))]
fn sstv_save_slot(i: usize, png_bytes: &[u8]) {
    if let Ok(dir) = sdroxide_config::sstv_tx_dir() {
        let _ = std::fs::write(dir.join(format!("slot{i}.png")), png_bytes);
    }
}
#[cfg(target_arch = "wasm32")]
fn sstv_save_slot(_i: usize, _png_bytes: &[u8]) {}

#[cfg(not(target_arch = "wasm32"))]
fn sstv_save_messages(messages: &[String]) {
    let _ = sdroxide_config::save_sstv_messages(messages);
}
#[cfg(target_arch = "wasm32")]
fn sstv_save_messages(_messages: &[String]) {}

#[cfg(not(target_arch = "wasm32"))]
fn fsq_load_contacts() -> Vec<sdroxide_types::FsqContact> {
    sdroxide_config::load_contacts()
}
#[cfg(target_arch = "wasm32")]
fn fsq_load_contacts() -> Vec<sdroxide_types::FsqContact> {
    Vec::new()
}

#[cfg(not(target_arch = "wasm32"))]
fn fsq_save_contacts(contacts: &[sdroxide_types::FsqContact]) {
    let _ = sdroxide_config::save_contacts(contacts);
}
#[cfg(target_arch = "wasm32")]
fn fsq_save_contacts(_contacts: &[sdroxide_types::FsqContact]) {}

#[cfg(not(target_arch = "wasm32"))]
fn sstv_load_messages() -> Vec<String> {
    sdroxide_config::load_sstv_messages()
}
#[cfg(target_arch = "wasm32")]
fn sstv_load_messages() -> Vec<String> {
    Vec::new()
}

#[cfg(not(target_arch = "wasm32"))]
fn sstv_load_slots() -> Vec<Option<(Vec<u8>, u16, u16)>> {
    let mut out = Vec::new();
    let dir = match sdroxide_config::sstv_tx_dir() {
        Ok(d) => d,
        Err(_) => return (0..5).map(|_| None).collect(),
    };
    for i in 0..5 {
        let entry = std::fs::read(dir.join(format!("slot{i}.png")))
            .ok()
            .and_then(|b| crate::sstv::load_source_bounded(&b, 1024));
        out.push(entry);
    }
    out
}
#[cfg(target_arch = "wasm32")]
fn sstv_load_slots() -> Vec<Option<(Vec<u8>, u16, u16)>> {
    (0..5).map(|_| None).collect()
}

#[cfg(not(target_arch = "wasm32"))]
fn sstv_load_gallery() -> Vec<(Vec<u8>, u16, u16)> {
    let mut entries: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(dir) = sdroxide_config::sstv_rx_dir() {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) == Some("png") {
                    entries.push(p);
                }
            }
        }
    }
    // Newest first by filename (timestamps), cap the count.
    entries.sort();
    entries.reverse();
    entries.truncate(40);
    entries
        .into_iter()
        .filter_map(|p| std::fs::read(&p).ok().and_then(|b| crate::sstv::decode_image(&b)))
        .collect()
}
#[cfg(target_arch = "wasm32")]
fn sstv_load_gallery() -> Vec<(Vec<u8>, u16, u16)> {
    Vec::new()
}

/// A titled, bordered section box of a fixed size, for the SSTV panel's LIVE /
/// RECEIVED / TRANSMIT areas.
fn sstv_section<R>(
    ui: &mut egui::Ui,
    title: &str,
    size: egui::Vec2,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    // Force a top-down layout: `allocate_ui` would otherwise inherit the parent's
    // horizontal layout (we're inside a `horizontal_top`), laying the section's
    // contents out side by side instead of stacked.
    ui.allocate_ui_with_layout(size, egui::Layout::top_down(egui::Align::Min), |ui| {
        egui::Frame::new()
            .fill(crate::theme::ROW_BG)
            .stroke(egui::Stroke::new(1.0, crate::theme::LINE_LIT))
            .inner_margin(egui::Margin { left: 8, right: 8, top: 5, bottom: 7 })
            .show(ui, |ui| {
                ui.set_min_size(egui::vec2(size.x - 16.0, size.y - 12.0));
                ui.set_max_width(size.x - 16.0);
                ui.label(RichText::new(title).size(9.5).strong().color(crate::theme::CYAN_DIM));
                ui.add_space(3.0);
                add(ui)
            })
            .inner
    })
    .inner
}

/// A small horizontal signal-activity meter (level ~0..1), so the operator can
/// confirm receive audio is reaching the SSTV decoder.
fn sstv_level_bar(ui: &mut egui::Ui, level: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(90.0, 10.0), egui::Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 2.0, Color32::from_gray(20));
    // Log scale (~ -60..0 dBFS mean-abs) so weak-but-decodable signals still show.
    let db = 20.0 * level.max(1e-6).log10();
    let frac = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
    let mut fill = rect;
    fill.set_width(rect.width() * frac);
    let col = if frac > 0.06 { crate::theme::GREEN } else { Color32::from_gray(45) };
    p.rect_filled(fill, 2.0, col);
    p.rect_stroke(
        rect,
        2.0,
        egui::Stroke::new(1.0, Color32::from_gray(60)),
        egui::StrokeKind::Inside,
    );
}
