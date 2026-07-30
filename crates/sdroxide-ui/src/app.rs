use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui::{self, Color32, ComboBox, DragValue, RichText, Slider};
use sdroxide_types::{
    AgcMode, AudioDevices, Band, CallsignInfo, Command, Decode, DeviceCaps, DigiStatus, Direction,
    GainElement, LookupProvider, MemoryChannel, Meters, Mode, NetworkConfig, QsoRecord,
    RadioController, RadioEvent, RadioState, RifpEncoding, RifpMeta, RifpProfile, RifpSize,
    RifpStatus, RxId, SkimmerKind, SkimmerSpot, SpectrumConfig, SpectrumFrame, Spot, SpotKind,
    SstvMode, SstvStatus, UploadResult, UploadTarget, Vfo,
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
    Tle,
}

/// Transient state of the TLE tab — what is in the paste box, which rows are
/// unfolded. Not persisted: none of it is a setting, it is where the operator
/// happens to be in the dialog.
#[derive(Default)]
struct SatEditState {
    /// The "paste element sets here" box.
    paste: String,
    /// Catalogue number and name for a new frequency entry.
    new_freq_id: String,
    new_freq_name: String,
    /// Index of the pasted element set whose two lines are shown for editing.
    open_tle: Option<usize>,
    /// Index of the frequency entry whose links are shown for editing.
    open_freq: Option<usize>,
    /// What the last add attempt did, good or bad, so a paste that yielded
    /// nothing says so instead of appearing to have been ignored.
    note: String,
}

/// One subscription's fetch state, in a form both targets have.
///
/// The native type lives in `sdroxide-solar`, which the browser build does not
/// compile the fetching half of; copying the three fields the dialog shows
/// keeps the tab itself target-agnostic.
#[derive(Clone, Default)]
struct SubStatusView {
    url: String,
    fetched_unix: i64,
    count: usize,
    /// How many of the listing's satellites are in the built-in curated list.
    /// Zero for everything that is not the amateur group.
    curated: usize,
    error: Option<String>,
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
    /// Re-enumerate the USB bus for RTL-SDR dongles. Cheap and non-invasive —
    /// no device is opened — so it cannot disturb a running stream.
    rtlsdr_rescan: &'a mut bool,
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
    /// The operator's satellite additions, and the transient state of the
    /// dialog that edits them. Persisted on change, like the input bindings:
    /// there is no APPLY step to hang it off.
    sat_edit: &'a mut sdroxide_types::SatConfig,
    sat_ui: &'a mut SatEditState,
    sat_subs: &'a [SubStatusView],
    /// Fetch every subscription now. Blocking, so it is done after the window
    /// closure the way the HPSDR scan is.
    sat_sub_refresh: &'a mut bool,
    /// How the 3D view draws its cloud deck: `Some(true)` marches the volume,
    /// `Some(false)` stacks shells through it. `None` where there is no 3D view
    /// to set it for — the browser client, whose solar view is a separate tab
    /// with its own settings — because a switch that provably does nothing is
    /// worse than no switch.
    solar_cloud_march: Option<&'a mut bool>,
    /// Reload the broadcast station list from disk, and restore the bundled one
    /// over the top of it. Both act on a file rather than on an edit buffer, so
    /// they are done after the window closure like the HPSDR scan.
    bc_reload: &'a mut bool,
    bc_restore: &'a mut bool,
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
fn auto_upload_adif(
    cfg: &NetworkConfig,
    rec: &QsoRecord,
) -> Option<(u64, String, Vec<UploadTarget>)> {
    if !cfg.auto_upload {
        return None;
    }
    let targets = auto_upload_targets(cfg);
    if targets.is_empty() {
        return None;
    }
    Some((rec.id, sdroxide_types::qso_log_to_adif(std::slice::from_ref(rec)), targets))
}

/// Index of a spot kind into the app's `spot_kinds_shown` filter array. Must
/// stay in lockstep with the chip order in [`App::spots_window`], which indexes
/// the array positionally.
fn spot_kind_index(kind: SpotKind) -> usize {
    match kind {
        SpotKind::DxCluster => 0,
        SpotKind::Pota => 1,
        SpotKind::Sota => 2,
        SpotKind::PskReporter => 3,
        SpotKind::FreeDv => 4,
        SpotKind::Broadcast => 5,
    }
}

/// Number of spot-kind filter chips, i.e. the width of `spot_kinds_shown`.
const SPOT_KINDS: usize = 6;

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

/// One decode as the list draws it: the entry itself, plus everything the row
/// needs resolved once up front — how far away they are, whether this is a CQ
/// *we* may answer, what working them would be worth against the log, and
/// whether the message names our own station.
#[derive(Clone, Copy)]
struct DecodeRow<'a> {
    /// Index into `digi_decodes`, so rows keep their identity after sorting.
    idx: usize,
    d: &'a Decode,
    dist_km: Option<f64>,
    cq: bool,
    novelty: sdroxide_types::Novelty,
    to_me: bool,
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
    rtlsdr_devices: Vec<sdroxide_types::RtlSdrDevice>,
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
    /// QSOs worked since this run started, which is what the FT8 panel's
    /// "Session" readout counts. Deliberately not derived from the logbook: the
    /// log is persisted and grows for ever, so counting it would report every
    /// contact ever made as if it had just been worked.
    session_qsos: usize,
    show_digi_settings: bool,
    /// UI-owned editable copy of the operator config, so typing isn't fought
    /// by the round-tripped status echo. Seeded once from the first status.
    digi_cfg_edit: sdroxide_types::DigiConfig,
    digi_cfg_seeded: bool,
    /// SSTV image-mode panel state (gallery, TX slots, message, textures).
    sstv: SstvUi,
    /// RF Paint (Spectrum Painting) panel state (text/image + previews).
    rf_paint: RfPaintUi,
    /// Hellschreiber receive raster (scrollback ring + texture).
    hell: crate::hell::HellUi,
    /// FSQ directed-message target callsign ("" = broadcast/ALLCALL).
    fsq_target: String,
    /// JS8: the `To:` callsign the composer addresses. Also what the globe
    /// draws the QSO arc to, JS8 having no QSO sequencer to ask instead.
    js8_target: String,
    /// JS8: callsigns a locator has already been requested for this session,
    /// successful or not. Every lookup is an HTTP round trip on its own thread,
    /// and a busy band puts fifty stations in the heard list.
    js8_looked_up: std::collections::HashSet<String>,
    /// JS8: frame time of the last locator lookup, so they go out one at a time
    /// rather than fifty at once the moment the panel opens.
    js8_lookup_at: f64,
    /// JS8: the last message we transmitted. What `AGN?` — "say again" — is
    /// asking for, and the one reply the operator cannot retype from memory.
    js8_last_sent: String,
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
    /// FREEDV, BC) — indexed by [`spot_kind_index`].
    spot_kinds_shown: [bool; SPOT_KINDS],
    /// Show only spots that fall inside the current panadapter view span.
    spot_in_view_only: bool,
    /// Fuzzy search query for the spot list. Narrows the list in the SPOTS
    /// window only — the waterfall labels are positioned by frequency, so
    /// reordering them by match quality would mean nothing.
    spot_search: String,
    /// The bundled/user broadcast station table, loaded once at startup.
    broadcast: Vec<sdroxide_types::BroadcastStation>,
    /// The subset of `broadcast` on air right now, as spots. Rebuilt when the
    /// UTC minute rolls over — the finest granularity a schedule changes at —
    /// rather than every frame.
    broadcast_spots: Vec<Spot>,
    /// The UTC minute `broadcast_spots` was built for.
    broadcast_minute: i64,
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
    /// Everything callsign lookup has resolved this session, by callsign. Kept
    /// because a JS8 station's locator usually never arrives on the air —
    /// only heartbeats and CQs carry one — so the map has nothing else to
    /// place the rest of the conversation by.
    callsign_cache: std::collections::HashMap<String, CallsignInfo>,
    /// QSO uploads queued (id, single-record ADIF, targets), drained to commands.
    pending_uploads: Vec<(u64, String, Vec<UploadTarget>)>,
    /// Awards dashboard open state + band filter ("" = all bands).
    show_awards: bool,
    awards_band: String,
    /// Cached award tally, keyed by (log length, band filter).
    awards_cache: Option<(usize, String, sdroxide_types::Awards)>,
    /// The same tally placed on the globe for the 3D view's award layer, keyed
    /// the same way. Shared rather than copied: it is three hundred entities
    /// and the window republishes it every frame.
    awards_heat: Option<(usize, String, Arc<Vec<sdroxide_types::EntitySlot>>)>,
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
    /// The operator's satellite additions: element sets they pasted in or
    /// subscribed to, and their frequency corrections. Shared by `Arc` because
    /// the solar window's render closure takes a handle it outlives any borrow
    /// of; replaced wholesale on every edit rather than mutated in place.
    sat_cfg: std::sync::Arc<sdroxide_types::SatConfig>,
    /// The settings dialog's working copy, its transient state, and what each
    /// subscription's last fetch did. All seeded when the dialog opens.
    sat_cfg_edit: sdroxide_types::SatConfig,
    sat_ui: SatEditState,
    sat_sub_status: Vec<SubStatusView>,
    /// Weather fax: the chart being painted and the gallery of saved ones.
    wefax: crate::wefax::WefaxUi,
    /// Whether the operator has dismissed the out-of-band transmit warning
    /// this session. Never persisted: `--oob-tx` has to be passed again on the
    /// next launch, so the warning has to be acknowledged again too.
    oob_tx_ack: bool,
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
            freq_mhz: if r.freq_hz > 0.0 {
                format!("{:.4}", r.freq_hz / 1e6)
            } else {
                String::new()
            },
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
        let band = if freq_hz > 0.0 {
            sdroxide_types::adif_band(freq_hz).to_string()
        } else {
            String::new()
        };
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
        let view: ViewState =
            cc.storage.and_then(|s| eframe::get_value(s, "view")).unwrap_or_default();
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
            rtlsdr_devices: Vec::new(),
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
            session_qsos: 0,
            show_digi_settings: false,
            digi_cfg_edit: sdroxide_types::DigiConfig::default(),
            sstv: SstvUi::default(),
            rf_paint: RfPaintUi::default(),
            hell: Default::default(),
            fsq_target: String::new(),
            js8_target: String::new(),
            js8_looked_up: Default::default(),
            js8_lookup_at: 0.0,
            js8_last_sent: String::new(),
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
            spot_kinds_shown: [true; SPOT_KINDS],
            spot_in_view_only: false,
            spot_search: String::new(),
            broadcast: load_broadcast_stations(),
            broadcast_spots: Vec::new(),
            broadcast_minute: -1,
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
            callsign_cache: Default::default(),
            pending_uploads: Vec::new(),
            show_awards: false,
            awards_band: String::new(),
            awards_cache: None,
            awards_heat: None,
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
            sat_cfg: std::sync::Arc::new(load_sat_config()),
            sat_cfg_edit: Default::default(),
            sat_ui: Default::default(),
            sat_sub_status: Vec::new(),
            wefax: Default::default(),
            oob_tx_ack: false,
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
        let mut traffic = self.digi_stations.traffic(
            now_t,
            status.and_then(|s| s.dx_grid.as_deref()),
            self.digi_preview.as_ref().map(|(_, ll)| *ll),
            status.is_some_and(|s| s.transmitting),
        );
        // JS8 has no QSO sequencer to ask for `dx_grid`, because a chat has no
        // Tx1–Tx6 to be part-way through. What an operator means by "in a QSO"
        // there is the station the composer is aimed at, so that is what gets
        // the arc — highlighted exactly as an FT8 contact in progress is.
        if self.state.rx[0].mode.is_js8() {
            let heard =
                status.and_then(|s| s.js8.as_ref()).map(|j| j.heard.as_slice()).unwrap_or(&[]);
            traffic.dx = self
                .js8_grid_for(&self.js8_target, heard)
                .as_deref()
                .and_then(sdroxide_types::grid_to_latlon);
        }
        // Weather fax has no callsign and no grid to place a station by, but it
        // does have a transmitter with a known location — so the chart being
        // received gets the same path across the globe a QSO would, which turns
        // an anonymous picture into "this came 900 km over the North Sea".
        if self.state.rx[0].mode.is_wefax()
            && let Some((st, _)) = sdroxide_types::WefaxStation::at_dial(self.state.rx_freq_hz())
        {
            traffic.dx = Some((st.lat, st.lon));
            traffic.dx_label = Some(st.name.to_string());
        }
        // A broadcast station is the same case again: no callsign, but a known
        // transmitter site, so tuning one draws the path the signal actually
        // travelled. Only when nothing else has claimed the arc — a QSO in
        // progress outranks whatever the dial happens to be sitting on — and
        // deliberately not gated on AM, because plenty of shortwave listening is
        // done in ECSS on one sideband.
        if traffic.dx.is_none()
            && let Some((st, lat, lon)) = sdroxide_types::broadcast::at_dial(
                &self.broadcast,
                self.state.rx_freq_hz(),
                now_unix(),
            )
            .and_then(|st| Some((st, st.lat?, st.lon?)))
        {
            traffic.dx = Some((lat, lon));
            traffic.dx_label = Some(if st.site.is_empty() {
                st.name.clone()
            } else {
                format!("{} · {}", st.name, st.site)
            });
        }
        traffic
    }

    /// The operator's grid square. Prefers the engine's copy but falls back to
    /// the UI's edit buffer: `digi_status` only arrives once the engine sends
    /// its first `DigiStatus`, and never at all in sessions with no digi engine.
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
        let dt =
            if self.wf_last_now > 0.0 { (now - self.wf_last_now).clamp(0.0, 0.3) } else { 0.0 };
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
        ui.with_layout(egui::Layout::left_to_right(egui::Align::Min).with_main_wrap(true), |ui| {
            self.freq_module(ui, cmds);
            self.smeter_module(ui);
            self.vfo_rit_module(ui, cmds);
            self.rx_filter_module(ui, cmds);
            // Only while the sub is running: the module appearing is itself the
            // confirmation that SUB took effect, and it costs a wrapped row of
            // top bar that operators who never use it should not have to pay.
            if self.state.sub_rx_enabled {
                self.sub_rx_module(ui, cmds);
            }
            if self.caps.as_ref().is_some_and(|c| c.is_transmit_capable()) {
                self.tx_module(ui, cmds);
            }
            self.display_module(ui, cmds);
            self.windows_module(ui);
        });
    }

    /// The VFO frequency controls (A/B select + big readout + the inactive
    /// VFO's frequency) in a label-less box, always the first module.
    fn freq_module(&mut self, ui: &mut egui::Ui, cmds: &mut Vec<Command>) {
        // The 10-digit readout is fixed width, so measure it (via the same fonts
        // freq_display uses) and size the box to hug its contents — that keeps the
        // right column against the box edge (no empty space) and lets the readout
        // be centred vertically by exact geometry rather than a fragile layout hint.
        let font40 = egui::FontId::monospace(40.0);
        let digit =
            ui.painter().layout_no_wrap("0".to_owned(), font40.clone(), Color32::WHITE).size();
        let dot_w = ui.painter().layout_no_wrap(".".to_owned(), font40, Color32::WHITE).size().x;
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
    /// cycles the needle / bar / trace faces.
    fn smeter_module(&mut self, ui: &mut egui::Ui) {
        crate::chrome::module_bare_flush_h(ui, 250.0, crate::chrome::MODULE_TALL_H, |ui| {
            let resp = smeter::show(ui, self.meters.as_ref(), self.view.smeter_style)
                .on_hover_text("Click to cycle meter face: needle / bar / trace");
            if resp.clicked() {
                self.view.smeter_style = self.view.smeter_style.next();
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
        // The Hell raster is a continuous strip with no frame boundary, so
        // leaving it up across a mode change would splice unrelated text.
        self.hell.clear();
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

    /// Whether a spot passes the operator's filters.
    ///
    /// The single place that decides this. The waterfall overlay, the SPOTS list
    /// and the world-map dots all go through here, so switching a category off
    /// cannot take effect in one view and be forgotten in another.
    ///
    /// The search query is deliberately *not* part of this: it narrows the list
    /// in the SPOTS window only. See [`App::spot_search`].
    fn spot_visible(&self, s: &Spot) -> bool {
        if !self.spot_kinds_shown[spot_kind_index(s.kind)] {
            return false;
        }
        if self.spot_in_view_only
            && !(self.view.view_lo_hz..=self.view.view_hi_hz).contains(&s.freq_hz)
        {
            return false;
        }
        true
    }

    /// Rebuild the on-air broadcast station list if the UTC minute has rolled
    /// over since it was last built. Cheap enough to call every frame.
    fn refresh_broadcast_spots(&mut self, now_utc: i64) {
        let minute = now_utc.div_euclid(60);
        if minute == self.broadcast_minute {
            return;
        }
        self.broadcast_minute = minute;
        self.broadcast_spots = sdroxide_types::broadcast::on_air(&self.broadcast, now_utc);
    }

    /// Live network spots and the on-air broadcast stations, unfiltered.
    fn all_spots(&self) -> impl Iterator<Item = &Spot> {
        self.spots.iter().chain(self.broadcast_spots.iter())
    }

    /// The same two sets as one owned list in frequency order, for the SPOTS
    /// window. `self.spots` arrives sorted from the feed manager, but the
    /// broadcast stations have to be merged into that order.
    fn merged_spots(&self) -> Vec<Spot> {
        let mut all: Vec<Spot> = self.all_spots().cloned().collect();
        all.sort_by(|a, b| a.freq_hz.total_cmp(&b.freq_hz));
        all
    }

    /// The network-spot overlay: the currently-shown spots (filtered by kind and,
    /// optionally, to the panadapter view span) plus a parallel age-fade alpha.
    /// Newest spots are solid; they dim over the last quarter of their lifetime.
    ///
    /// Runs every frame, so it clones only what survives the filters rather than
    /// building a merged list first — the layout pass sorts by screen position
    /// itself, so the output need not be in frequency order.
    fn net_overlay(&self, now_utc: i64) -> (Vec<Spot>, Vec<f32>) {
        let max_age = self.net_cfg_edit.spot_max_age_secs.max(60) as i64;
        let mut spots = Vec::new();
        let mut alpha = Vec::new();
        for s in self.all_spots() {
            if !self.spot_visible(s) {
                continue;
            }
            // A scheduled broadcast station has no age: the fade and the
            // max-age cut are both about how stale a *report* is, and a
            // transmitter that is on the air now is not a stale report.
            let a = if s.kind == SpotKind::Broadcast {
                1.0
            } else {
                let age = (now_utc - s.when_utc).max(0);
                if age > max_age {
                    continue;
                } else if age as f64 > max_age as f64 * 0.75 {
                    (1.0 - (age as f64 - max_age as f64 * 0.75) / (max_age as f64 * 0.25)) as f32
                } else {
                    1.0
                }
            };
            spots.push(s.clone());
            alpha.push(a.clamp(0.15, 1.0));
        }
        (spots, alpha)
    }

    /// Open a fresh log entry pre-filled from a clicked spot, and kick a
    /// callsign lookup if auto-lookup is on.
    ///
    /// Broadcast stations are exempt: "BBC World Service" is not a callsign to
    /// log or look up on QRZ, so clicking one only tunes. Guarding here rather
    /// than at each call site covers both the SPOTS list and the panadapter.
    fn prefill_from_spot(&mut self, spot: &Spot) {
        if spot.kind == SpotKind::Broadcast {
            return;
        }
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
    ///
    /// Returns whether one was actually queued, so a caller that rations
    /// lookups can tell "asked" from "lookup is switched off" and not burn its
    /// one-per-interval budget on a request that never left.
    fn queue_lookup(&mut self, call: String) -> bool {
        let call = call.trim().to_string();
        if call.is_empty()
            || !self.net_cfg_edit.auto_lookup
            || self.net_cfg_edit.lookup_provider == LookupProvider::None
        {
            return false;
        }
        self.pending_lookups.push(call);
        true
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
        // Keep the whole record: the JS8 map places stations by the grid in it,
        // and that station may never have a log entry to merge into.
        self.callsign_cache.insert(info.call.to_ascii_uppercase(), info.clone());

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
                self.log_content_changed();
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
            self.log_content_changed();
        }
        self.push_net_log(format!(
            "Confirmations: {} downloaded, {matched} newly confirmed",
            recs.len()
        ));
    }

    /// Drop everything derived from the logbook.
    ///
    /// The caches below key on the log's *length*, which catches a QSO being
    /// added or deleted but not one being edited in place — and a confirmation
    /// arriving, or a lookup filling in a grid, is exactly that. Without this
    /// the awards tally (and the globe's heat layer, which is the same tally
    /// placed on the Earth) would keep showing yesterday's answer until the
    /// next QSO happened to change the length.
    fn log_content_changed(&mut self) {
        self.awards_cache = None;
        self.awards_heat = None;
        self.worked_entities_cache = None;
        self.log_index_cache = None;
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
        let stale =
            self.awards_cache.as_ref().map(|(l, b, _)| *l != len || *b != band).unwrap_or(true);
        if stale {
            let filter = (!band.is_empty()).then_some(band.as_str());
            let awards = sdroxide_types::compute_awards(&self.qso_log, filter, None);
            self.awards_cache = Some((len, band, awards));
        }
    }

    /// Award coverage placed on the globe, for the 3D view's award layer. Built
    /// from the same tally the dashboard shows and cached the same way, so the
    /// two can never tell different stories about the same log.
    #[cfg(not(target_arch = "wasm32"))]
    fn award_heat(&mut self) -> Arc<Vec<sdroxide_types::EntitySlot>> {
        let len = self.qso_log.len();
        let band = self.awards_band.clone();
        let stale =
            self.awards_heat.as_ref().map(|(l, b, _)| *l != len || *b != band).unwrap_or(true);
        if stale {
            self.ensure_awards();
            let slots = self
                .awards_cache
                .as_ref()
                .map(|(_, _, a)| sdroxide_types::entity_coverage(a))
                .unwrap_or_default();
            self.awards_heat = Some((len, band, Arc::new(slots)));
        }
        Arc::clone(&self.awards_heat.as_ref().expect("just filled").2)
    }

    /// The awards dashboard: DXCC / WAS / WAZ / grid counts (worked vs
    /// confirmed) with a band filter, plus the WAS state grid and WAZ zone grid.
    /// The out-of-band transmit warning.
    ///
    /// Modal and dismissed by hand, because the band-edge lockout is the last
    /// thing between a mistyped frequency and an out-of-band transmission, and
    /// an operator who does not know it is off is exactly the operator who will
    /// find out the expensive way. Dismissing it is a one-shot acknowledgement,
    /// not a preference: it comes back next launch, because the flag has to be
    /// passed again next launch.
    ///
    /// Driven off the *engine's* state rather than off this process's arguments
    /// so a remote client is warned too — the licence at risk belongs to
    /// whoever is at the controls, who need not be whoever started the engine.
    fn oob_tx_window(&mut self, ctx: &egui::Context) {
        if !self.state.oob_tx || self.oob_tx_ack {
            return;
        }
        let mut dismissed = false;
        let resp = egui::Window::new("⚠  TRANSMIT LOCKOUT DISABLED")
            .frame(crate::chrome::window_frame())
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(430.0);
                ui.label(
                    RichText::new(
                        "This engine was started with --oob-tx. The amateur-band lockout is \
                         off: it will key the transmitter on any frequency the hardware \
                         supports.",
                    )
                    .color(crate::theme::TEXT_STRONG)
                    .size(13.0),
                );
                ui.add_space(6.0);
                ui.label(
                    RichText::new(
                        "Transmitting outside your licence is an offence in every country that \
                         issues one. Only continue if you are authorised to use the frequencies \
                         you are about to key on — a MARS/CAP or commercial licence, an \
                         experimental permit, or a dummy load.",
                    )
                    .color(crate::theme::TEXT),
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if crate::chrome::chip_accent(
                        ui,
                        false,
                        RichText::new("  I UNDERSTAND  ").strong(),
                        crate::theme::PINK,
                        crate::theme::TEXT_STRONG,
                    )
                    .clicked()
                    {
                        dismissed = true;
                    }
                    ui.label(
                        RichText::new("Restart without --oob-tx to put the lockout back.")
                            .color(crate::theme::LINE_LIT)
                            .size(10.5),
                    );
                });
            });
        if let Some(r) = &resp {
            crate::chrome::paint_window_border(ctx, &r.response);
        }
        if dismissed {
            self.oob_tx_ack = true;
        }
    }

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
                    ui.label(
                        RichText::new("Worked All States")
                            .size(12.0)
                            .strong()
                            .color(crate::theme::CYAN),
                    );
                    award_cell_grid(
                        ui,
                        sdroxide_types::US_STATES.iter().map(|s| {
                            (s.to_string(), awards.was.get(*s).copied().unwrap_or_default())
                        }),
                        44.0,
                    );
                    ui.add_space(8.0);
                    // WAZ zone grid (1..40).
                    ui.label(
                        RichText::new("CQ Zones (WAZ)")
                            .size(12.0)
                            .strong()
                            .color(crate::theme::CYAN),
                    );
                    award_cell_grid(
                        ui,
                        (1u8..=40).map(|z| {
                            (format!("{z:02}"), awards.waz.get(&z).copied().unwrap_or_default())
                        }),
                        34.0,
                    );
                    ui.add_space(8.0);
                    // DXCC worked list (confirmed marked).
                    ui.label(
                        RichText::new("DXCC entities")
                            .size(12.0)
                            .strong()
                            .color(crate::theme::CYAN),
                    );
                    for (name, st) in &awards.dxcc {
                        let col =
                            if st.confirmed { crate::theme::GREEN } else { crate::theme::YELLOW };
                        ui.label(
                            RichText::new(format!(
                                "{} {name}",
                                if st.confirmed { "✓" } else { "•" }
                            ))
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
                    if crate::chrome::chip(ui, false, "A↔B").on_hover_text("Swap VFOs").clicked()
                    {
                        cmds.push(Command::SwapVfos);
                    }
                    if crate::chrome::chip(ui, false, "A→B").on_hover_text("Copy A to B").clicked()
                    {
                        cmds.push(Command::CopyAtoB);
                    }
                    if crate::chrome::chip(ui, self.state.split, "SPLIT").clicked() {
                        cmds.push(Command::SetSplit(!self.state.split));
                    }
                    if crate::chrome::chip(ui, self.state.sub_rx_enabled, "SUB")
                        .on_hover_text(
                            "Second receiver, in the right ear. It tunes independently of \
                             A/B — its controls appear in the SUB module, and its passband \
                             on the waterfall.",
                        )
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
                                Some(hz) => {
                                    cmds.push(Command::SetVfo { vfo: self.state.active_vfo, hz })
                                }
                                None => cmds.push(Command::SetBand(b)),
                            }
                        }
                    }
                });
                ui.add_space(6.0);
                ui.label(RichText::new("MODE").color(crate::theme::CYAN_DIM).size(9.5).strong());
                ui.horizontal_wrapped(|ui| {
                    for m in [
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
                    ] {
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
        // The device's front-end RX gain, if it has one the software can set —
        // the Hermes-Lite 2's LNA, a SoapySDR device's first RX stage. A rig
        // with none (a CAT radio on a sound card) gets no slider and no extra
        // module width, so nothing moves for the people who can't use it.
        let rx_gains: Vec<GainElement> = self
            .caps
            .as_ref()
            .map(|c| c.gains.iter().filter(|g| g.direction == Direction::Rx).cloned().collect())
            .unwrap_or_default();
        let rx_gain = rx_gains.first().cloned();
        let width = if rx_gain.is_some() { 506.0 } else { 356.0 };
        crate::chrome::module_bare_h(ui, width, crate::chrome::MODULE_TALL_H, |ui| {
            ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(5.0, 5.0);
                // Receiver: volume, RF gain, AGC, mute.
                ui.horizontal(|ui| {
                    let mut vol = self.state.rx[0].volume;
                    ui.label("Vol");
                    if crate::chrome::slider(ui, Slider::new(&mut vol, 0.0..=1.0).show_value(false))
                        .changed()
                    {
                        self.state.rx[0].volume = vol; // optimistic echo
                        cmds.push(Command::SetVolume { rx: RxId::Main, v: vol });
                    }
                    if let Some(g) = &rx_gain {
                        let mut hint = format!(
                            "Front-end RX gain ({}). Too much clips the receiver's ADC and \
                             smears spurious signals across the band; too little and it goes deaf.",
                            g.name
                        );
                        if rx_gains.len() > 1 {
                            hint.push_str(&format!(
                                "\n\nThis rig has {} RX gain stages — the rest are in \
                                 Settings → Device.",
                                rx_gains.len()
                            ));
                        }
                        ui.label("Gain").on_hover_text(&hint);
                        let mut db = self
                            .state
                            .gains
                            .iter()
                            .find(|(n, _)| *n == g.name)
                            .map(|(_, d)| *d)
                            .unwrap_or(g.min_db);
                        let step = if g.step_db > 0.0 { g.step_db } else { 1.0 };
                        // Narrower rail than Vol: this one carries a dB readout,
                        // and the module has to stay inside one wrapped row.
                        let resp = ui
                            .scope(|ui| {
                                ui.spacing_mut().slider_width = 76.0;
                                crate::chrome::slider(
                                    ui,
                                    Slider::new(&mut db, g.min_db..=g.max_db)
                                        .step_by(step)
                                        .suffix(" dB"),
                                )
                            })
                            .inner
                            .on_hover_text(&hint);
                        if resp.changed() {
                            // Optimistic echo so the knob tracks the drag instead
                            // of snapping back until the engine answers.
                            match self.state.gains.iter_mut().find(|(n, _)| *n == g.name) {
                                Some((_, d)) => *d = db,
                                None => self.state.gains.push((g.name.clone(), db)),
                            }
                            cmds.push(Command::SetGain {
                                dir: Direction::Rx,
                                element: g.name.clone(),
                                db,
                            });
                        }
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
                    // WFM broadcast stereo: lit while a 19 kHz pilot is locked,
                    // click to force mono. Only WFM has a pilot to find.
                    if self.state.rx[0].mode == Mode::Wfm {
                        let want = self.state.rx[0].wfm_stereo;
                        let locked = self.meters.as_ref().is_some_and(|m| m.stereo);
                        let hover = if !want {
                            "WFM stereo forced off — click for automatic stereo"
                        } else if locked {
                            "WFM stereo: pilot locked. Click to force mono"
                        } else {
                            "WFM stereo: automatic, no pilot on this station"
                        };
                        if crate::chrome::chip(ui, want && locked, "ST")
                            .on_hover_text(hover)
                            .clicked()
                        {
                            self.state.rx[0].wfm_stereo = !want; // optimistic echo
                            cmds.push(Command::SetWfmStereo { rx: RxId::Main, on: !want });
                        }
                    }
                });
            });
        });
    }

    /// The sub receiver's own controls, shown only while it is running. The sub
    /// has a frequency, a mode and a filter of its own — none of which the main
    /// receiver's controls can reach — so without this module it is a second
    /// receiver that can only be switched on and off.
    fn sub_rx_module(&mut self, ui: &mut egui::Ui, cmds: &mut Vec<Command>) {
        // The sub tunes anywhere inside the device passband and nowhere outside
        // it: both receivers are DDCs on the same IQ stream.
        let half = self.state.sample_rate / 2.0;
        let (dev_lo, dev_hi) = (self.state.center_hz - half, self.state.center_hz + half);
        // Field height, and the height every row is told to be. egui sizes a
        // horizontal row from `interact_size.y` and then grows it as taller
        // widgets land in it — which drops everything added after the first
        // chip a few pixels below everything added before it. Starting the row
        // at the height its tallest widget will be leaves nothing to grow.
        const FIELD_H: f32 = 22.0;
        crate::chrome::module_bare_h(ui, 404.0, crate::chrome::MODULE_TALL_H, |ui| {
            ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(5.0, 5.0);
                ui.spacing_mut().interact_size.y = FIELD_H;
                // Frequency, mode, and the two moves worth a single click:
                // send the sub to the dial, or bring the dial to the sub.
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("SUB")
                            .color(crate::widgets::spectrum_view::SUB_COLOR)
                            .size(11.0)
                            .strong(),
                    );
                    let mut hz = self.state.sub_rx_hz;
                    let resp = ui
                        .add_sized(
                            [116.0, FIELD_H],
                            DragValue::new(&mut hz)
                                .speed(10.0)
                                .range(dev_lo..=dev_hi)
                                // Typed and shown in MHz — the unit the operator
                                // reads a frequency in — while the drag step
                                // stays in Hz so it tunes like a dial.
                                .custom_formatter(|v, _| format!("{:.6}", v / 1e6))
                                .custom_parser(|s| s.trim().parse::<f64>().ok().map(|m| m * 1e6))
                                .suffix(" MHz"),
                        )
                        .on_hover_text(
                            "Where the sub receiver listens. Shift-click the waterfall, or \
                             drag inside the sub's passband, to move it.",
                        );
                    if resp.changed() {
                        self.state.sub_rx_hz = hz; // optimistic echo
                        cmds.push(Command::SetSubRxFreq(hz));
                    }
                    let mode = self.state.rx[1].mode;
                    ComboBox::from_id_salt("sub-mode")
                        .selected_text(mode.label())
                        .width(74.0)
                        .show_ui(ui, |ui| {
                            // Audio modes only. The digital modes are wired to
                            // the main receiver alone (one decoder, one TX), and
                            // SPEC produces no audio at all — a sub receiver you
                            // cannot hear is a trap, not a setting.
                            for m in [
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
                            ] {
                                if ui.selectable_label(mode == m, m.label()).clicked() {
                                    cmds.push(Command::SetMode { rx: RxId::Sub, mode: m });
                                }
                            }
                        });
                    if crate::chrome::chip(ui, false, "←DIAL")
                        .on_hover_text("Move the sub receiver to the main dial")
                        .clicked()
                    {
                        cmds.push(Command::SetSubRxFreq(self.state.rx_freq_hz()));
                    }
                    if crate::chrome::chip(ui, false, "DIAL←")
                        .on_hover_text("Move the main dial to the sub receiver")
                        .clicked()
                    {
                        cmds.push(Command::SetVfo {
                            vfo: self.state.active_vfo,
                            hz: self.state.sub_rx_hz,
                        });
                    }
                });
                // Filter, level, mute.
                ui.horizontal(|ui| {
                    let rx1 = self.state.rx[1];
                    let max = rx1.mode.max_filter_hz();
                    ui.label("Filter").on_hover_text("Sub receiver passband edges, in Hz");
                    let mut lo = rx1.filter_lo;
                    let mut hi = rx1.filter_hi;
                    let changed = ui
                        .add_sized(
                            [70.0, FIELD_H],
                            DragValue::new(&mut lo).speed(10).range(-max..=max),
                        )
                        .changed()
                        | ui.add_sized(
                            [70.0, FIELD_H],
                            DragValue::new(&mut hi).speed(10).range(-max..=max),
                        )
                        .changed();
                    if changed {
                        // Same 50 Hz floor the waterfall grips enforce, so the
                        // passband can't be dragged shut from either route.
                        let (lo, hi) = (lo.min(hi - 50.0), hi.max(lo + 50.0));
                        (self.state.rx[1].filter_lo, self.state.rx[1].filter_hi) = (lo, hi);
                        cmds.push(Command::SetFilter { rx: RxId::Sub, lo, hi });
                    }
                    let mut vol = rx1.volume;
                    ui.label("Vol").on_hover_text("Sub receiver level (it plays in the right ear)");
                    if ui
                        .scope(|ui| {
                            ui.spacing_mut().slider_width = 64.0;
                            crate::chrome::slider(
                                ui,
                                Slider::new(&mut vol, 0.0..=1.0).show_value(false),
                            )
                        })
                        .inner
                        .changed()
                    {
                        self.state.rx[1].volume = vol; // optimistic echo
                        cmds.push(Command::SetVolume { rx: RxId::Sub, v: vol });
                    }
                    if crate::chrome::chip_accent(
                        ui,
                        rx1.muted,
                        "MUTE",
                        crate::theme::PINK,
                        Color32::WHITE,
                    )
                    .clicked()
                    {
                        cmds.push(Command::SetMute { rx: RxId::Sub, muted: !rx1.muted });
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
                        sdroxide_types::slot_label(i as usize, &self.voice.slot(i as usize).name)
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
                ui.label(
                    RichText::new("SKIMMERS").color(crate::theme::CYAN_DIM).size(9.5).strong(),
                );
                // Edit a copy and send the whole struct on any change; the
                // engine echoes it back in the next RadioState.
                let mut cfg = self.state.skimmer;
                // A grid so the squelch fields line up under each other despite
                // the kind chips having different widths.
                egui::Grid::new("skimmer-kinds").num_columns(3).spacing([6.0, 5.0]).show(
                    ui,
                    |ui| {
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
                                .add(
                                    DragValue::new(&mut sql)
                                        .speed(0.25)
                                        .range(0..=40)
                                        .suffix(" dB"),
                                )
                                .on_hover_text("Minimum SNR a decoded signal needs to be spotted")
                                .changed()
                            {
                                cfg.set_squelch_db(kind, sql);
                            }
                            ui.end_row();
                        }
                    },
                );
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
                    ui.label(
                        RichText::new("WATERFALL").color(crate::theme::CYAN_DIM).size(9.5).strong(),
                    );
                    if crate::chrome::chip(ui, self.view.waterfall_flip, "FLIP")
                        .on_hover_text(
                            "Scroll the waterfall upwards — newest row at the bottom (V)",
                        )
                        .clicked()
                    {
                        self.view.waterfall_flip = !self.view.waterfall_flip;
                    }
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
            let hresp = crate::chrome::split_handle(ui, egui::vec2(handle_w, avail.y), None);
            if hresp.dragged() {
                let d = hresp.drag_delta().x / avail.x.max(1.0);
                self.view.digi_split_fraction =
                    (self.view.digi_split_fraction + d).clamp(0.28, 0.72);
            }
            ui.vertical(|ui| {
                self.qso_area(ui, cmds);
            });
        });
    }

    /// The band's other conventional frequencies for this mode, as a chip that
    /// opens a picker.
    ///
    /// Only appears where there is actually a choice. Most modes have one
    /// agreed frequency per band and the chip would be a button that does
    /// nothing; the ones that have several — FT8's DXpedition window, PSK and
    /// RTTY's region split, SSTV's move-up-when-busy convention — are exactly
    /// the ones where an operator otherwise has to go and look the number up.
    ///
    /// The dial is what moves. These are dial frequencies, and the audio
    /// offset within the passband is a separate control that must not be
    /// disturbed by changing band segment.
    fn digi_freq_chip(&self, ui: &mut egui::Ui, cmds: &mut Vec<Command>) {
        let mode = self.state.rx[0].mode;
        let dial = self.state.active_freq_hz();
        let band = sdroxide_types::Band::containing(dial);
        let channels = sdroxide_types::digi_channels_in(mode, band);
        if channels.len() < 2 {
            return;
        }
        // "On" when the dial is already sitting on one of them, so the chip
        // doubles as a readout of whether you are where the mode expects.
        let here = channels.iter().find(|c| (c.dial_hz - dial).abs() < 1.0);
        let face = match here {
            Some(c) => format!("⇵ {:.3}", c.dial_hz / 1e6),
            None => "⇵ FREQ".to_string(),
        };
        let btn = crate::chrome::chip(ui, here.is_some(), RichText::new(face).size(11.0))
            .on_hover_text(format!(
                "The {} frequencies agreed for {} on {}",
                channels.len(),
                mode.label(),
                band.label()
            ));

        let mut pick = None;
        let resp = egui::Popup::from_toggle_button_response(&btn)
            .frame(crate::chrome::window_frame())
            .close_behavior(egui::PopupCloseBehavior::CloseOnClick)
            .show(|ui| {
                ui.set_max_width(300.0);
                ui.label(
                    RichText::new(format!("{} · {}", mode.label(), band.label()))
                        .color(crate::theme::CYAN_DIM)
                        .size(9.5)
                        .strong(),
                );
                ui.add_space(2.0);
                for c in &channels {
                    let on = here.map(|h| h.dial_hz) == Some(c.dial_hz);
                    let mut text = format!("{:.3} MHz", c.dial_hz / 1e6);
                    if !c.note.is_empty() {
                        text.push_str(&format!("   {}", c.note));
                    }
                    let mut rich = RichText::new(text).size(12.0);
                    if c.outside_r1_data_segment(mode) {
                        rich = rich.color(crate::theme::YELLOW);
                    }
                    let row = ui.selectable_label(on, rich);
                    if c.outside_r1_data_segment(mode) {
                        row.clone().on_hover_text(
                            "A global convention that the IARU Region 1 band plan does not put \
                             narrow data on — check your own band plan before transmitting here.",
                        );
                    }
                    if row.clicked() {
                        pick = Some(c.dial_hz);
                    }
                }
                if channels.iter().any(|c| c.outside_r1_data_segment(mode)) {
                    ui.add_space(2.0);
                    ui.label(
                        RichText::new("Amber: outside the Region 1 data segment.")
                            .color(crate::theme::LINE_LIT)
                            .size(10.0),
                    );
                }
            });
        if let Some(r) = &resp {
            crate::chrome::paint_popup_cut_border(ui.ctx(), &r.response, 1.0);
        }
        if let Some(hz) = pick {
            cmds.push(Command::SetVfo { vfo: self.state.active_vfo, hz });
        }
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
            for (m, base) in [
                (DecodeSort::None, "None"),
                (DecodeSort::Signal, "SNR"),
                (DecodeSort::Distance, "Dist"),
            ] {
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
            if crate::chrome::chip(ui, self.digi_cq_only, "CQ only")
                .on_hover_text(
                    "Only stations calling CQ — and only the calls you may answer: a directed \
                     CQ (DX, EU, JA, POTA, TEST …) is listed when it names you and hidden when \
                     it names someone else.",
                )
                .clicked()
            {
                self.digi_cq_only = !self.digi_cq_only;
            }
            if crate::chrome::chip(ui, self.digi_new_only, "New only")
                .on_hover_text("Only stations that would be new: entity, band-slot, grid, or call")
                .clicked()
            {
                self.digi_new_only = !self.digi_new_only;
            }
            // Whether the engine chooses our transmit frequency. Here rather
            // than in the setup window because it decides what clicking a
            // decode in this list does.
            if self.digi_cfg_seeded {
                let auto = self.digi_cfg_edit.auto_tx_freq;
                if crate::chrome::chip(ui, auto, "Auto TX FRQ")
                    .on_hover_text(
                        "Pick our transmit frequency automatically: the quietest spot in the \
                         period we transmit in, rather than the frequency of whoever we are \
                         answering — they transmit in the other period, so theirs says nothing \
                         about who is there when we key. Off holds the frequency you set.",
                    )
                    .clicked()
                {
                    self.digi_cfg_edit.auto_tx_freq = !auto;
                    cmds.push(Command::SetDigiConfig(self.digi_cfg_edit.clone()));
                }
            }
        });
        ui.add_space(2.0);
        // Call of the currently previewed decode (cloned so the scroll closure
        // doesn't hold a borrow of `self` we need to write back afterwards).
        let preview_call = self.digi_preview.as_ref().map(|(c, _)| c.clone());
        // Own grid, for the per-decode great-circle distance column.
        let my_grid =
            self.digi_status.as_ref().map(|s| s.config.my_grid.clone()).unwrap_or_default();
        // Own callsign, to spotlight decodes addressed to us.
        let my_call =
            self.digi_status.as_ref().map(|s| s.config.my_call.clone()).unwrap_or_default();
        // Stations already marked to work, so their queue button reads as set.
        let queued_calls: Vec<String> = self
            .digi_status
            .as_ref()
            .map(|s| s.call_queue.iter().map(|q| q.call.clone()).collect())
            .unwrap_or_default();
        // Staged preview change: `None` = no click this frame; `Some(v)` =
        // replace the preview with `v` (`Some(None)` clears it).
        let mut new_preview: Option<Option<(String, (f64, f64))>> = None;
        // Location of the row hovered this frame → yellow dot on the map.
        let mut hover_ll: Option<(f64, f64)> = None;
        let cq_only = self.digi_cq_only;
        let new_only = self.digi_new_only;
        let auto_tx_freq = self.digi_status.as_ref().map(|s| s.config.auto_tx_freq).unwrap_or(true);
        let sort = self.digi_sort;
        let desc = self.digi_sort_desc;
        // Turn parity needs the mode's slot length (FT8 15 s, FT4 7.5 s). JS8's
        // is an operator setting rather than implied by the mode, so it comes
        // from the status — otherwise Turbo draws one turn header per 2.5 turns.
        let period = self.slot_period_s();
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
        let mut items: Vec<DecodeRow> = self
            .digi_decodes
            .iter()
            .enumerate()
            .filter_map(|(i, d)| {
                // A message addressed to our own station survives every filter.
                // It is the one decode in the list we owe an answer to, and
                // hiding it behind "CQ only" — a station calling us back is not
                // calling CQ — leaves no REPLY button anywhere to answer from.
                let to_me = !my_call.is_empty() && d.to.as_deref() == Some(my_call.as_str());
                let cq = sdroxide_types::cq_is_for_us(d, &my_call, &my_grid);
                if cq_only && !cq && !to_me {
                    return None;
                }
                let novelty =
                    log_ix.novelty(d.from.as_deref().unwrap_or(""), d.grid.as_deref(), band);
                if new_only && !novelty.is_new() && !to_me {
                    return None;
                }
                let dist_km = (!my_grid.is_empty())
                    .then(|| {
                        d.grid
                            .as_deref()
                            .and_then(|g| sdroxide_types::grid_distance_km(&my_grid, g))
                    })
                    .flatten();
                Some(DecodeRow { idx: i, d, dist_km, cq, novelty, to_me })
            })
            .collect();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show_themed(ui, |ui| {
            let mut gi = 0;
            while gi < items.len() {
                // A turn is one slot: group the contiguous same-slot decodes.
                let slot = items[gi].d.slot_utc;
                let mut end = gi;
                while end < items.len() && items[end].d.slot_utc == slot {
                    end += 1;
                }
                match sort {
                    DecodeSort::None => {}
                    DecodeSort::Signal => items[gi..end].sort_by(|a, b| {
                        let o = a.d.snr_db.cmp(&b.d.snr_db);
                        if desc { o.reverse() } else { o }
                    }),
                    DecodeSort::Distance => items[gi..end].sort_by(|a, b| {
                        // Decodes without a grid always sort last (push them to the
                        // far end of whichever direction is active).
                        let sentinel = if desc { f64::NEG_INFINITY } else { f64::INFINITY };
                        let ka = a.dist_km.unwrap_or(sentinel);
                        let kb = b.dist_km.unwrap_or(sentinel);
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
                    let DecodeRow { idx: i, d, dist_km, cq, novelty, to_me } = items[k];
                    // Free text names no sender, and a hashed callsign nobody
                    // has heard yet resolves to none either — say which it is
                    // rather than showing a bare "?".
                    let who = d
                        .from
                        .clone()
                        .unwrap_or_else(|| if d.free_text { "TEXT".into() } else { "?".into() });
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
                    // Where in the world they are, from the callsign alone —
                    // most decodes carry no grid, and the entity always knows.
                    let entity = d.from.as_deref().and_then(sdroxide_types::resolve_callsign);
                    let continent = entity.map(|e| e.continent).unwrap_or("");
                    let is_preview =
                        d.from.is_some() && preview_call.as_deref() == d.from.as_deref();
                    let queued = d
                        .from
                        .as_deref()
                        .is_some_and(|f| queued_calls.iter().any(|q| q.eq_ignore_ascii_case(f)));
                    let mut reply = false;
                    let mut queue = false;
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
                                let cell =
                                    |ui: &mut egui::Ui,
                                     w: f32,
                                     align_right: bool,
                                     lbl: egui::Label| {
                                        row_cell(ui, w, ch, align_right, lbl)
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
                                // Continent — the band's opening, readable down
                                // the column without reading a single callsign.
                                cell(
                                    ui,
                                    24.0,
                                    false,
                                    egui::Label::new(
                                        RichText::new(continent)
                                            .monospace()
                                            .size(11.0)
                                            .strong()
                                            .color(if dupe {
                                                Color32::from_gray(85)
                                            } else {
                                                crate::theme::continent_color(continent)
                                            }),
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
                                // Message fills the remaining width; REPLY and the
                                // queue button pinned right.
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
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
                                        // Mark for later. Pressing it again drops
                                        // the station, so one button both queues
                                        // and un-queues.
                                        let qresp = crate::chrome::chip(
                                            ui,
                                            queued,
                                            RichText::new(if queued { "＋" } else { "+" })
                                                .size(12.0)
                                                .strong(),
                                        )
                                        .on_hover_text(if queued {
                                            "Queued — click to remove"
                                        } else {
                                            "Work this station after the current one"
                                        });
                                        queue = qresp.clicked();
                                        reply_left = Some(resp.rect.left().min(qresp.rect.left()));
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
                                    },
                                );
                            });
                        });

                    let r = inner.response.rect;
                    // Left-accent bar: gold (to us) / red (CQ) / cyan (other). Wider
                    // for a to-us decode so it really pops — and for a directed CQ
                    // (DX, EU, JA …) that names us, which is a better prospect than
                    // a plain CQ anyone in the world is free to answer.
                    let (accent, aw) = if to_me {
                        (crate::theme::YELLOW, 4.0)
                    } else if cq {
                        (crate::theme::PINK, if d.cq_to.is_some() { 4.0 } else { 2.5 })
                    } else {
                        (crate::theme::CYAN_DIM, 2.5)
                    };
                    ui.painter().rect_filled(
                        egui::Rect::from_min_max(
                            r.left_top(),
                            egui::pos2(r.left() + aw, r.bottom()),
                        ),
                        0.0,
                        accent,
                    );
                    // Row-body click (everything left of the REPLY button) tunes
                    // the audio freq. Excluding the button's rect keeps this
                    // interaction from covering — and stealing clicks from — REPLY.
                    let body_right = reply_left.map(|x| x - 2.0).unwrap_or(r.right());
                    let body_rect =
                        egui::Rect::from_min_max(r.left_top(), egui::pos2(body_right, r.bottom()));
                    let row =
                        ui.interact(body_rect, ui.id().with(("dec", i)), egui::Sense::click());
                    // Everything already resolved about this station, gathered
                    // where there is room to say it: the row itself has to fit
                    // twenty of these on screen.
                    let row = row.on_hover_ui(|ui| {
                        station_card(ui, d, entity, dist_km, &my_grid, novelty, band, queued, cq);
                    });
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
                    } else if queue {
                        if let Some(from) = &d.from {
                            if queued {
                                cmds.push(Command::DigiQueueRemove(from.clone()));
                            } else {
                                cmds.push(Command::DigiQueueAdd {
                                    from: from.clone(),
                                    grid: d.grid.clone(),
                                    snr: d.snr_db,
                                    audio_hz: d.audio_hz,
                                    // Same judgement the reply button makes: a
                                    // station mid-exchange is not free yet.
                                    wait_for_cq: !d.is_cq && !to_me,
                                });
                            }
                        }
                    } else if row.clicked() {
                        // Moving our transmit onto theirs is exactly what Auto
                        // TX FRQ exists to avoid, so with it on a click only
                        // previews the station.
                        if !auto_tx_freq {
                            cmds.push(Command::SetDigiAudioFreq(d.audio_hz));
                        }
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
        // The count is this run's; the export buttons still save the whole
        // logbook, which is what their hover text says.
        let session = self.session_qsos;
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
            egui::UiBuilder::new()
                .max_rect(zone(0.0))
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
            |ui| {
                ui.label(RichText::new("QSO").size(9.5).strong().color(crate::theme::CYAN_DIM));
                self.digi_freq_chip(ui, cmds);
            },
        );
        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(zone(1.0))
                .layout(egui::Layout::top_down(egui::Align::Center)),
            |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("Session: {session} QSO"))
                            .size(11.0)
                            .color(Color32::from_gray(150)),
                    )
                    .on_hover_text("QSOs worked since sdroxide was started");
                    if ui
                        .add_enabled(logged > 0, egui::Button::new("ADIF"))
                        .on_hover_text(format!("Save the whole logbook ({logged} QSO) as ADIF"))
                        .clicked()
                    {
                        let adif = sdroxide_types::qso_log_to_adif(&self.qso_log);
                        crate::download::save("sdroxide-log.adi", adif.as_bytes());
                    }
                    if ui
                        .add_enabled(logged > 0, egui::Button::new("TXT"))
                        .on_hover_text(format!("Save the whole logbook ({logged} QSO) as text"))
                        .clicked()
                    {
                        let txt = sdroxide_types::qso_log_to_text(&self.qso_log);
                        crate::download::save("sdroxide-log.txt", txt.as_bytes());
                    }
                });
            },
        );
        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(zone(2.0))
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
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
        let map_hi =
            (full_h - (map_handle_h + CARD_RESERVE + 5.0 + gap + btn_h)).min(avail_w).max(map_lo);
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
        self.digi_stations.observe(&self.digi_decodes, now_t, now_unix());
        let stations = self.digi_stations.stations(now_t);
        // Located network spots (filtered by the shown-kind toggles), as
        // kind-coloured dots on the map.
        //
        // FreeDV Reporter is excluded whatever its toggle says: it lists every
        // station currently *connected*, hundreds of them, which buries the
        // decoded FT8 stations this map exists to show. The panadapter overlay
        // and the SPOTS window still carry them.
        // Broadcast stations do appear here: they carry real transmitter
        // coordinates, and the on-air filter keeps their count in the same range
        // as the cluster spots already drawn.
        let spot_dots: Vec<(f64, f64, (u8, u8, u8))> = self
            .all_spots()
            .filter(|s| s.kind != SpotKind::FreeDv)
            .filter(|s| self.spot_visible(s))
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
            let hresp = crate::chrome::split_handle(
                ui,
                egui::vec2(ui.available_width(), map_handle_h),
                None,
            );
            if hresp.dragged() {
                // 1:1 with the cursor: a drag of `dy` px moves the map edge `dy` px.
                let df = hresp.drag_delta().y / (map_hi - map_lo).max(1.0);
                self.view.digi_map_fraction = (self.view.digi_map_fraction + df).clamp(0.0, 1.0);
            }
        }
        // Station card.
        crate::chrome::red_panel(ui, |ui| {
            match status.as_ref() {
                Some(s) => {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(s.step.label())
                                .size(13.0)
                                .strong()
                                .color(crate::theme::CYAN),
                        );
                        if s.transmitting {
                            ui.label(
                                RichText::new("● TX").size(13.0).strong().color(crate::theme::PINK),
                            );
                        }
                        if s.config.dxped_mode != sdroxide_types::DxpedMode::Normal
                            && s.mode == Mode::Ft8
                        {
                            ui.label(
                                RichText::new(s.config.dxped_mode.label().to_uppercase())
                                    .size(11.0)
                                    .strong()
                                    .color(crate::theme::PINK),
                            )
                            .on_hover_text(
                                "DXpedition mode. The transmit frequency is held out of the \
                                 Fox's half of the passband (below 1000 Hz) until the Fox \
                                 answers.",
                            );
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
                            // What everyone else's timing says about ours. A
                            // clock far enough out that nobody can decode us
                            // looks exactly like a dead band from this side, so
                            // it is worth a permanent readout.
                            if let Some(off) = s.clock_offset_s {
                                use sdroxide_types::ClockHealth::*;
                                let health = sdroxide_types::clock_health(off);
                                let col = match health {
                                    Good => Color32::from_gray(140),
                                    Marginal => crate::theme::YELLOW,
                                    Bad => crate::theme::PINK,
                                };
                                let txt = RichText::new(format!("DT {off:+.1} s")).size(11.0);
                                ui.label(if health == Good {
                                    txt.color(col)
                                } else {
                                    txt.strong().color(col)
                                })
                                .on_hover_text(format!(
                                    "Your slot timing against the stations you are hearing.\n\
                                     {}\n\nIt covers the whole receive path, so a slow audio or \
                                     network chain counts the same as a wrong clock. Under 0.5 s \
                                     is comfortable.",
                                    match health {
                                        Good => "Well inside tolerance.".to_string(),
                                        _ => format!(
                                            "You transmit {:.1} s {} everyone else — the usual \
                                             reason calls go unanswered. Sync your computer clock.",
                                            off.abs(),
                                            if off > 0.0 { "before" } else { "after" },
                                        ),
                                    }
                                ));
                            }
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
                                    ui.label(
                                        RichText::new(g).size(13.0).color(crate::theme::CYAN_DIM),
                                    );
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
                    ui.label(
                        RichText::new("FT8 engine idle").size(12.0).color(Color32::from_gray(130)),
                    );
                }
            }
        });

        // Fox pile-up: who is being worked and who is waiting. Only a Fox has
        // one, so its presence is the mode indicator.
        let fox_queue = status.as_ref().map(|s| s.fox_queue.clone()).unwrap_or_default();
        if !fox_queue.is_empty() {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.label(
                    RichText::new(format!("PILE-UP {}", fox_queue.len()))
                        .size(9.5)
                        .strong()
                        .color(crate::theme::CYAN_DIM),
                );
                for c in &fox_queue {
                    let col = if c.working { crate::theme::GREEN } else { Color32::from_gray(150) };
                    ui.label(RichText::new(&c.call).size(11.5).strong().color(col)).on_hover_text(
                        format!(
                            "{} · {:+} dB{}",
                            if c.working { "being worked" } else { "waiting" },
                            c.snr_db,
                            c.grid.as_deref().map(|g| format!(" · {g}")).unwrap_or_default(),
                        ),
                    );
                }
            });
        }

        // The call queue: stations marked to be worked, in the order they will
        // be taken. Clicking one drops it; CLEAR empties the lot.
        let call_queue = status.as_ref().map(|s| s.call_queue.clone()).unwrap_or_default();
        if !call_queue.is_empty() {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.label(
                    RichText::new(format!("QUEUE {}", call_queue.len()))
                        .size(9.5)
                        .strong()
                        .color(crate::theme::CYAN_DIM),
                );
                for (i, q) in call_queue.iter().enumerate() {
                    // The one going next is the one worth reading first.
                    let col = if i == 0 { crate::theme::GREEN } else { Color32::from_gray(150) };
                    if crate::chrome::chip(ui, false, RichText::new(&q.call).size(11.5).color(col))
                        .on_hover_text(format!(
                            "{} · {:+} dB · {:.0} Hz{}\nClick to remove",
                            if i == 0 { "next" } else { "waiting" },
                            q.snr_db,
                            q.audio_hz,
                            q.grid.as_deref().map(|g| format!(" · {g}")).unwrap_or_default(),
                        ))
                        .clicked()
                    {
                        cmds.push(Command::DigiQueueRemove(q.call.clone()));
                    }
                }
                if crate::chrome::chip(ui, false, RichText::new("CLEAR").size(10.0)).clicked() {
                    cmds.push(Command::DigiQueueRemove(String::new()));
                }
            });
        }

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
                    crate::chrome::chip(ui, step_now == Some(step), RichText::new(label).size(11.0))
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
                RichText::new(format!("{audio_hz:.0} Hz"))
                    .size(11.0)
                    .color(Color32::from_gray(150)),
            );
            if crate::chrome::chip(ui, false, "−").on_hover_text("Tune down 10 Hz").clicked() {
                cmds.push(Command::SetDigiAudioFreq((audio_hz - 10.0).clamp(200.0, 3500.0)));
            }
            if crate::chrome::chip(ui, false, "+").on_hover_text("Tune up 10 Hz").clicked() {
                cmds.push(Command::SetDigiAudioFreq((audio_hz + 10.0).clamp(200.0, 3500.0)));
            }
            self.digi_freq_chip(ui, cmds);
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
                        .show_themed(ui, |ui| {
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
                        });
                });
        });
        ui.add_space(gap);

        // TX input: already-sent characters are coloured green via a layouter.
        let prev = self.text_tx.clone();
        let sent = sent.min(prev.chars().count());
        let prefix: String = prev.chars().take(sent).collect();
        let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, wrap: f32| {
            let text = buf.as_str();
            let sent_byte = text.char_indices().nth(sent).map(|(i, _)| i).unwrap_or(text.len());
            let mut job = egui::text::LayoutJob::default();
            job.wrap.max_width = wrap;
            let mono = egui::FontId::monospace(13.0);
            if sent_byte > 0 {
                job.append(
                    &text[..sent_byte],
                    0.0,
                    egui::TextFormat {
                        font_id: mono.clone(),
                        color: crate::theme::GREEN,
                        ..Default::default()
                    },
                );
            }
            if sent_byte < text.len() {
                job.append(
                    &text[sent_byte..],
                    0.0,
                    egui::TextFormat {
                        font_id: mono.clone(),
                        color: crate::theme::TEXT_STRONG,
                        ..Default::default()
                    },
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

    /// Hellschreiber panel: the scrolling receive raster on top, then a
    /// streaming TX input (already-sent characters green) and controls.
    ///
    /// Laid out like [`Self::text_modem_panel`] — Hell types the same way — but
    /// where that shows decoded text this shows the raster, because Hell carries
    /// pictures of letters rather than letters.
    fn hell_panel(&mut self, ui: &mut egui::Ui, cmds: &mut Vec<Command>, panel_h: f32) {
        let content_bottom = ui.cursor().top() + panel_h - 40.0;
        let status = self.digi_status.clone();
        let audio_hz = status.as_ref().map(|s| s.audio_hz).unwrap_or(1500.0);
        let sent = status.as_ref().map(|s| s.tx_sent).unwrap_or(0);
        let tx_on = status.as_ref().map(|s| s.tx_next).unwrap_or(false);
        let transmitting = status.as_ref().map(|s| s.transmitting).unwrap_or(false);
        let my_call = status.as_ref().map(|s| s.config.my_call.clone()).unwrap_or_default();
        let variant = status.as_ref().map(|s| s.config.hell_variant).unwrap_or_default();

        // Header: mode + tuning readout / nudges, variant chips, TX indicator.
        ui.horizontal(|ui| {
            ui.label(RichText::new("HELL").size(11.0).strong().color(crate::theme::CYAN));
            ui.label(
                RichText::new(format!("{audio_hz:.0} Hz"))
                    .size(11.0)
                    .color(Color32::from_gray(150)),
            );
            if crate::chrome::chip(ui, false, "−").on_hover_text("Tune down 10 Hz").clicked() {
                cmds.push(Command::SetDigiAudioFreq(audio_hz - 10.0));
            }
            if crate::chrome::chip(ui, false, "+").on_hover_text("Tune up 10 Hz").clicked() {
                cmds.push(Command::SetDigiAudioFreq(audio_hz + 10.0));
            }
            self.digi_freq_chip(ui, cmds);
            self.hell_params_row(ui, cmds);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if transmitting {
                    ui.label(RichText::new("● TX").size(11.0).strong().color(crate::theme::PINK));
                }
                self.digi_squelch_slider(ui, cmds);
            });
        });
        ui.add_space(4.0);

        // Raster appearance + scale. All client-side, so none of it round-trips
        // through the engine.
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let v = &mut self.view.hell;
            ui.label(RichText::new("Contrast").size(10.5).color(crate::theme::CYAN_DIM));
            ui.spacing_mut().slider_width = 70.0;
            ui.add(egui::Slider::new(&mut v.contrast, 0.4..=3.0).show_value(false))
                .on_hover_text("Harder or softer dots — redraws the whole strip");
            ui.add_space(6.0);
            ui.label(RichText::new("Width").size(10.5).color(crate::theme::CYAN_DIM));
            for px in [1.0f32, 2.0, 3.0, 4.0] {
                let sel = (v.col_px - px).abs() < 0.01;
                if ui.selectable_label(sel, format!("{px:.0}×")).clicked() {
                    v.col_px = px;
                }
            }
            ui.add_space(6.0);
            if crate::chrome::chip(ui, v.doubled, " 2ROW ")
                .on_hover_text(
                    "Draw every column twice, stacked. Hell has no vertical sync, so this \
                     keeps one complete copy of the text readable whatever the phase.",
                )
                .clicked()
            {
                v.doubled = !v.doubled;
            }
            if crate::chrome::chip(ui, v.reverse, " REV ")
                .on_hover_text("Reverse video — light dots on dark paper")
                .clicked()
            {
                v.reverse = !v.reverse;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if crate::chrome::chip(ui, false, " CLEAR RX ").clicked() {
                    self.hell.clear();
                }
                ui.label(
                    RichText::new(format!("{:.1} char/s", variant.chars_per_sec()))
                        .size(10.5)
                        .color(Color32::from_gray(120)),
                );
            });
        });
        ui.add_space(4.0);

        // Reserve the input + button rows at the bottom; the raster gets the rest.
        let btn_h = 32.0;
        let input_h = 56.0;
        let gap = 5.0;
        let bottom_pad = 12.0;
        let rx_h = (content_bottom - ui.cursor().top() - btn_h - input_h - 2.0 * gap - bottom_pad)
            .max(28.0);

        let (rect, resp) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), rx_h),
            egui::Sense::click_and_drag(),
        );
        // Dragging lines the text up by hand when the doubled view is off.
        if resp.dragged() && !self.view.hell.doubled {
            let d = resp.drag_delta().y / rect.height().max(1.0);
            self.view.hell.valign = (self.view.hell.valign - d).rem_euclid(1.0);
        }
        if resp.hovered() && !self.view.hell.doubled {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
        }
        self.hell.draw(ui, rect, &self.view.hell);
        ui.add_space(gap);

        // TX input: already-sent characters coloured green, exactly as the
        // keyboard modes do it.
        let prev = self.text_tx.clone();
        let sent = sent.min(prev.chars().count());
        let prefix: String = prev.chars().take(sent).collect();
        let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, wrap: f32| {
            let text = buf.as_str();
            let sent_byte = text.char_indices().nth(sent).map(|(i, _)| i).unwrap_or(text.len());
            let mut job = egui::text::LayoutJob::default();
            job.wrap.max_width = wrap;
            let mono = egui::FontId::monospace(13.0);
            if sent_byte > 0 {
                job.append(
                    &text[..sent_byte],
                    0.0,
                    egui::TextFormat {
                        font_id: mono.clone(),
                        color: crate::theme::GREEN,
                        ..Default::default()
                    },
                );
            }
            if sent_byte < text.len() {
                job.append(
                    &text[sent_byte..],
                    0.0,
                    egui::TextFormat {
                        font_id: mono.clone(),
                        color: crate::theme::TEXT_STRONG,
                        ..Default::default()
                    },
                );
            }
            ui.fonts_mut(|f| f.layout_job(job))
        };
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
                            .id_salt("hell-tx")
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
            if !self.text_tx.starts_with(&prefix) {
                self.text_tx = prev;
            }
            cmds.push(Command::DigiTxText(self.text_tx.clone()));
        }
        ui.add_space(gap);

        ui.horizontal(|ui| {
            let label = if tx_on { "  TX ON  " } else { "   TX   " };
            if crate::chrome::chip_accent(
                ui,
                tx_on,
                RichText::new(label).size(14.0).strong(),
                crate::theme::PINK,
                Color32::WHITE,
            )
            .on_hover_text("Hold the channel: idle sends blank paper, so the strip keeps scrolling")
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
                let call = if my_call.is_empty() { "NOCALL".to_string() } else { my_call.clone() };
                let cq = format!("CQ CQ CQ DE {call} {call} {call} PSE K ");
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
                        let lbl = if (b - 45.45).abs() < 0.5 {
                            "45".to_string()
                        } else {
                            format!("{b:.0}")
                        };
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
                let lbl =
                    if (b - 4.5).abs() < 0.05 { "4.5".to_string() } else { format!("{b:.0}") };
                if ui.selectable_label(sel, lbl).clicked() {
                    cfg.fsq_baud = b;
                    changed = true;
                }
            }
            ui.add_space(8.0);
            ui.label(RichText::new("Call").size(10.5).strong().color(crate::theme::CYAN_DIM));
            if ui
                .add(egui::TextEdit::singleline(&mut cfg.fsq_call).desired_width(76.0))
                .on_hover_text(
                    "Callsign for directed (FSQCALL) messages; defaults to your callsign",
                )
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
    /// The JS8 panel: what is on the band, and the conversation.
    ///
    /// Shaped like the FT8 panel — an activity list on the left, a draggable
    /// split, a working area on the right — but the right-hand side is a chat
    /// log rather than a QSO sequencer, because that is what JS8 carries.
    fn js8_panel(&mut self, ui: &mut egui::Ui, cmds: &mut Vec<Command>, panel_h: f32) {
        use sdroxide_types::Js8Speed;

        let content_bottom = ui.cursor().top() + panel_h - 26.0;
        let status = self.digi_status.clone();
        let audio_hz = status.as_ref().map(|s| s.audio_hz).unwrap_or(1500.0);
        let transmitting = status.as_ref().map(|s| s.transmitting).unwrap_or(false);
        let js8 = status.as_ref().and_then(|s| s.js8.clone()).unwrap_or_default();

        // ── Header: speed, tuning, queue depth ──────────────────────────────
        ui.horizontal(|ui| {
            ui.label(RichText::new("JS8").size(11.0).strong().color(crate::theme::CYAN));
            for speed in Js8Speed::ALL {
                if crate::chrome::chip(ui, js8.speed == speed, speed.label()).clicked()
                    && js8.speed != speed
                {
                    self.digi_cfg_edit.js8_speed = speed;
                    cmds.push(Command::SetDigiConfig(self.digi_cfg_edit.clone()));
                }
            }
            ui.label(RichText::new(format!("{audio_hz:.0} Hz")).monospace());
            if crate::chrome::chip(ui, false, "−").clicked() {
                cmds.push(Command::SetDigiAudioFreq((audio_hz - 10.0).clamp(200.0, 3500.0)));
            }
            if crate::chrome::chip(ui, false, "+").clicked() {
                cmds.push(Command::SetDigiAudioFreq((audio_hz + 10.0).clamp(200.0, 3500.0)));
            }
            self.digi_freq_chip(ui, cmds);
            // Beacon state. An unattended transmitter must say so where the
            // operator is already looking, and say when it will key next — a
            // countdown is the difference between "armed" and "hung".
            let hb_min = self.digi_cfg_edit.js8_heartbeat_min;
            // Lit by what the engine is *doing*, not by what is configured: at
            // Turbo the interval is set and nothing beacons, and a chip that
            // claimed otherwise would be the one place this must not be wrong.
            let hb_on = crate::chrome::chip(ui, js8.next_hb_in_s.is_some(), "HB AUTO")
                .on_hover_text(match js8.next_hb_in_s {
                    Some(_) => format!("Beaconing every {hb_min} min — click to stop"),
                    None if js8.speed == Js8Speed::Turbo => {
                        "Turbo does not beacon — it is the local and VHF speed".to_string()
                    }
                    None => "Beacon your callsign and grid every 15 minutes".to_string(),
                })
                .clicked();
            if hb_on {
                // Off if it was on; otherwise the interval most of the band
                // uses, which SETUP can then change.
                self.digi_cfg_edit.js8_heartbeat_min = if hb_min > 0 { 0 } else { 15 };
                cmds.push(Command::SetDigiConfig(self.digi_cfg_edit.clone()));
            }
            if let Some(left) = js8.next_hb_in_s {
                ui.label(
                    RichText::new(format!("{}:{:02}", left / 60, left % 60))
                        .monospace()
                        .color(crate::theme::CYAN_DIM),
                )
                .on_hover_text("Until the next heartbeat");
            }
            // Beacons do not go out on the working frequency, so the waterfall
            // shows a burst where the panel's marker is not. Saying where it
            // went is the difference between that reading as a bug and as the
            // sub-band convention working.
            if let Some(hz) = js8.hb_hz {
                ui.label(
                    RichText::new(format!("HB {hz:.0} Hz")).monospace().color(crate::theme::GREEN),
                )
                .on_hover_text(format!(
                    "The last beacon went out at {hz:.0} Hz — a free slot in the {:.0}–{:.0} Hz \
                     heartbeat sub-band, chosen so it lands clear of the signals being decoded.",
                    sdroxide_types::HB_BAND_LO_HZ,
                    sdroxide_types::HB_BAND_HI_HZ,
                ));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Every setting this mode has — callsign, groups, auto-reply,
                // the beacon interval, the status message — lives in that
                // window, and the JS8 panel is the only one with no other way
                // in: FT8 reaches it from the QSO area, and the keyboard modes
                // keep their parameters in the header instead.
                if crate::chrome::chip(ui, self.show_digi_settings, "⚙ SETUP").clicked() {
                    self.show_digi_settings = !self.show_digi_settings;
                }
                if transmitting {
                    ui.label(RichText::new("● TX").color(crate::theme::PINK).strong());
                }
                // A long message takes minutes, not seconds. Saying so while it
                // is going out is the difference between "stuck" and "working".
                if js8.tx_frames_total > 0 {
                    let left = f64::from(js8.tx_frames_pending) * js8.speed.slot_s();
                    ui.label(
                        RichText::new(format!(
                            "{}/{} frames · {left:.0}s",
                            js8.tx_frames_total - js8.tx_frames_pending,
                            js8.tx_frames_total
                        ))
                        .monospace()
                        .color(crate::theme::YELLOW),
                    );
                }
                self.digi_squelch_slider(ui, cmds);
            });
        });
        ui.add_space(4.0);

        // Locate the heard stations and hand them to the maps. Done before the
        // list is drawn so a row and its dot on the globe agree this frame.
        let now_t = ui.input(|i| i.time);
        self.js8_observe(&js8.heard, now_t);

        let avail_h = (content_bottom - ui.cursor().top()).max(80.0);
        let total_w = ui.available_width();
        let left_w = (total_w * self.view.js8_split_fraction).clamp(160.0, total_w - 200.0);

        ui.horizontal_top(|ui| {
            // ── Left: who is on the band ────────────────────────────────────
            ui.vertical(|ui| {
                ui.set_width(left_w);
                ui.label(RichText::new("HEARD").size(10.5).strong().color(crate::theme::CYAN_DIM));
                self.js8_heard_list(ui, &js8, avail_h - 18.0, left_w);
            });

            // ── The drag handle ─────────────────────────────────────────────
            let resp = crate::chrome::split_handle(ui, egui::vec2(7.0, avail_h), None);
            if resp.dragged() {
                let dx = resp.drag_delta().x;
                self.view.js8_split_fraction = ((left_w + dx) / total_w).clamp(0.22, 0.72);
            }

            // ── Right: the conversation ─────────────────────────────────────
            //
            // Laid out bottom-up so the controls claim their real height and
            // the conversation takes whatever is left. Reserving a guessed
            // number of pixels for them instead clips the bottom row as soon as
            // a chip is added or the theme's spacing changes.
            ui.vertical(|ui| {
                ui.set_height(avail_h);
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                    // First declared is lowest in a bottom-up layout, so this
                    // is the gap between the controls and the panel edge.
                    // Without it they sit flush against the frame.
                    ui.add_space(8.0);
                    self.js8_compose(ui, cmds, &js8);
                    ui.add_space(4.0);
                    // Back to normal order for the scrolling part.
                    ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                        self.js8_conversation(ui, &js8);
                    });
                });
            });
        });
    }

    // ── JS8: locating stations ──────────────────────────────────────────────

    /// Where a JS8 station is, if anything knows.
    ///
    /// On-air first — a heartbeat, a CQ or a ` GRID` reply carries a real
    /// locator — then whatever callsign lookup resolved. Most JS8 traffic
    /// carries no grid at all, which is the whole reason the lookup path is
    /// here: without it the map would show only the stations that happened to
    /// beacon while we were listening.
    fn js8_grid_for(&self, call: &str, heard: &[sdroxide_types::Js8Heard]) -> Option<String> {
        if call.is_empty() {
            return None;
        }
        heard
            .iter()
            .find(|h| h.call.eq_ignore_ascii_case(call))
            .and_then(|h| h.grid.clone())
            .or_else(|| {
                self.callsign_cache.get(&call.to_ascii_uppercase()).and_then(|i| i.grid.clone())
            })
            .filter(|g| sdroxide_types::grid_to_latlon(g).is_some())
    }

    /// Feed the heard list to the maps, and ask the lookup service where the
    /// stations that never sent a locator actually are.
    ///
    /// The flat map and the globe both draw [`crate::digi_map::DigiStations`],
    /// which speaks in [`Decode`]s — so the heard stations are handed over in
    /// that shape rather than teaching the map a second kind of station.
    fn js8_observe(&mut self, heard: &[sdroxide_types::Js8Heard], now_t: f64) {
        // JS8's own convention is a heartbeat every ten or fifteen minutes, so
        // FT8's two-minute fade would leave the map blank between them.
        self.digi_stations.set_fade_s(JS8_STATION_FADE_S);
        let located: Vec<Decode> = heard
            .iter()
            .filter_map(|h| {
                let grid = self.js8_grid_for(&h.call, heard)?;
                Some(Decode {
                    slot_utc: h.last_utc,
                    snr_db: h.snr_db,
                    dt: 0.0,
                    audio_hz: h.audio_hz,
                    message: String::new(),
                    to: None,
                    from: Some(h.call.clone()),
                    grid: Some(grid),
                    is_cq: false,
                    cq_to: None,
                    rr73_to: None,
                    free_text: false,
                })
            })
            .collect();
        self.digi_stations.observe(&located, now_t, now_unix());

        // One lookup at a time. Each is an HTTP round trip on a thread of its
        // own, and a busy band puts fifty stations in this list at once.
        if now_t - self.js8_lookup_at < JS8_LOOKUP_INTERVAL_S {
            return;
        }
        let next = heard.iter().map(|h| h.call.to_ascii_uppercase()).find(|c| {
            !c.is_empty()
                && !c.starts_with('@')
                && !self.js8_looked_up.contains(c)
                && self.js8_grid_for(c, heard).is_none()
        });
        if let Some(call) = next {
            // Only spend the interval on a request that actually left: with no
            // provider configured this must stay ready for the moment one is.
            if self.queue_lookup(call.clone()) {
                self.js8_looked_up.insert(call);
                self.js8_lookup_at = now_t;
            }
        }
    }

    // ── JS8: the heard list ─────────────────────────────────────────────────

    /// Who is on the band, as the same styled rows the FT8 decode list uses.
    ///
    /// Deliberately the same shape — signal, frequency, callsign, what they'd
    /// be worth, where they are, and a REPLY button — because it is the same
    /// judgement being made, and an operator who has learned to read one list
    /// should not have to learn a second.
    fn js8_heard_list(
        &mut self,
        ui: &mut egui::Ui,
        js8: &sdroxide_types::Js8Status,
        max_h: f32,
        col_w: f32,
    ) {
        let my_grid = self.my_grid();
        let dial_hz = self.state.active_freq_hz();
        let band = if dial_hz > 0.0 { sdroxide_types::adif_band(dial_hz) } else { "" };
        self.log_index();
        // The last thing each station said: what the row shows, and what the
        // REPLY button drafts an answer to.
        let last_msg: std::collections::HashMap<&str, &sdroxide_types::Js8Msg> = js8
            .messages
            .iter()
            .filter(|m| !m.from.is_empty())
            .map(|m| (m.from.as_str(), m))
            .collect();
        let me = self.js8_me(js8);
        // Dropping the last three columns is what keeps the row readable when
        // the split is dragged narrow; the message then gets the space instead.
        let wide = col_w > 430.0;

        // Staged, because the row closures borrow `self` immutably.
        let mut pick: Option<(String, Option<String>)> = None;

        egui::ScrollArea::vertical()
            .id_salt("js8-heard")
            .max_height(max_h)
            .auto_shrink([false, false])
            .show_themed(ui, |ui| {
                if js8.heard.is_empty() {
                    ui.label(RichText::new("— nothing heard yet —").weak());
                }
                for (i, h) in js8.heard.iter().enumerate() {
                    let msg = last_msg.get(h.call.as_str()).copied();
                    let to_me = msg.is_some_and(js8_personally_addressed);
                    // A heartbeat or a CQ is an invitation, which is what FT8's
                    // red CQ row means too.
                    let calling =
                        msg.is_some_and(|m| matches!(m.cmd.as_deref(), Some("CQ") | Some("HB")));
                    let selected = self.js8_target.eq_ignore_ascii_case(&h.call);
                    let grid = self.js8_grid_for(&h.call, &js8.heard);
                    let dist_km = (!my_grid.is_empty())
                        .then(|| {
                            grid.as_deref()
                                .and_then(|g| sdroxide_types::grid_distance_km(&my_grid, g))
                        })
                        .flatten();
                    let entity = sdroxide_types::resolve_callsign(&h.call);
                    let continent = entity.map(|e| e.continent).unwrap_or("");
                    let novelty = self.log_index_cache.as_ref().expect("just refreshed").1.novelty(
                        &h.call,
                        grid.as_deref(),
                        band,
                    );
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
                    // A grid nobody sent is a guess from the callsign database,
                    // and the row says so rather than passing it off as heard.
                    let looked_up = grid.is_some() && h.grid.is_none();
                    let mut reply = false;
                    let mut reply_left: Option<f32> = None;

                    let inner = egui::Frame::new()
                        .fill(if to_me {
                            crate::theme::TOME_BG
                        } else if calling {
                            crate::theme::CQ_BG
                        } else {
                            crate::theme::ROW_BG
                        })
                        .inner_margin(egui::Margin { left: 11, right: 6, top: 6, bottom: 6 })
                        .show(ui, |ui| {
                            let ch = 22.0;
                            ui.horizontal(|ui| {
                                ui.set_min_height(ch);
                                ui.spacing_mut().item_spacing.x = 7.0;
                                row_cell(
                                    ui,
                                    28.0,
                                    ch,
                                    true,
                                    egui::Label::new(
                                        RichText::new(format!("{:+}", h.snr_db))
                                            .monospace()
                                            .size(13.0)
                                            .color(snr_color(h.snr_db)),
                                    ),
                                );
                                row_cell(
                                    ui,
                                    40.0,
                                    ch,
                                    true,
                                    egui::Label::new(
                                        RichText::new(format!("{:.0}", h.audio_hz))
                                            .monospace()
                                            .size(12.0)
                                            .color(Color32::from_gray(120)),
                                    ),
                                );
                                row_cell(
                                    ui,
                                    92.0,
                                    ch,
                                    false,
                                    egui::Label::new(
                                        RichText::new(&h.call).size(15.0).strong().color(
                                            if to_me {
                                                crate::theme::YELLOW
                                            } else if dupe {
                                                Color32::from_gray(105)
                                            } else if calling {
                                                crate::theme::GREEN
                                            } else {
                                                crate::theme::TEXT_STRONG
                                            },
                                        ),
                                    )
                                    .truncate(),
                                );
                                row_cell(
                                    ui,
                                    34.0,
                                    ch,
                                    false,
                                    egui::Label::new(
                                        RichText::new(badge).size(9.5).strong().color(badge_col),
                                    ),
                                );
                                if wide {
                                    row_cell(
                                        ui,
                                        24.0,
                                        ch,
                                        false,
                                        egui::Label::new(
                                            RichText::new(continent)
                                                .monospace()
                                                .size(11.0)
                                                .strong()
                                                .color(if dupe {
                                                    Color32::from_gray(85)
                                                } else {
                                                    crate::theme::continent_color(continent)
                                                }),
                                        ),
                                    );
                                    row_cell(
                                        ui,
                                        50.0,
                                        ch,
                                        false,
                                        egui::Label::new(
                                            RichText::new(grid.clone().unwrap_or_default())
                                                .monospace()
                                                .size(12.0)
                                                // Dimmer for a grid the database
                                                // supplied rather than the air.
                                                .color(if looked_up {
                                                    Color32::from_gray(110)
                                                } else {
                                                    crate::theme::CYAN_DIM
                                                }),
                                        ),
                                    );
                                    row_cell(
                                        ui,
                                        58.0,
                                        ch,
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
                                }
                                // What they last said fills the rest, with the
                                // REPLY button pinned right.
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        let resp = crate::chrome::chip_accent(
                                            ui,
                                            false,
                                            RichText::new("REPLY").size(12.0).strong(),
                                            if to_me {
                                                crate::theme::YELLOW
                                            } else if calling {
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
                                                let said =
                                                    msg.map(js8_msg_summary).unwrap_or_default();
                                                ui.add(
                                                    egui::Label::new(
                                                        RichText::new(said)
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
                                    },
                                );
                            });
                        });

                    let r = inner.response.rect;
                    let (accent, aw) = if to_me {
                        (crate::theme::YELLOW, 4.0)
                    } else if calling {
                        (crate::theme::PINK, 2.5)
                    } else {
                        (crate::theme::CYAN_DIM, 2.5)
                    };
                    ui.painter().rect_filled(
                        egui::Rect::from_min_max(
                            r.left_top(),
                            egui::pos2(r.left() + aw, r.bottom()),
                        ),
                        0.0,
                        accent,
                    );
                    let body_right = reply_left.map(|x| x - 2.0).unwrap_or(r.right());
                    let body =
                        egui::Rect::from_min_max(r.left_top(), egui::pos2(body_right, r.bottom()));
                    let row = ui
                        .interact(body, ui.id().with(("js8h", i)), egui::Sense::click())
                        .on_hover_ui(|ui| {
                            let d = js8_station_decode(h, grid.clone(), msg);
                            station_card(
                                ui, &d, entity, dist_km, &my_grid, novelty, band, false, calling,
                            );
                        });
                    if selected {
                        // The composer is aimed here: the same amber outline the
                        // FT8 list uses for the decode it is previewing.
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
                    // REPLY drafts the answer this exchange expects; a plain
                    // click only aims the composer, so it never overwrites a
                    // half-typed sentence.
                    if reply {
                        pick = Some((
                            h.call.clone(),
                            msg.and_then(|m| js8_reply_for(m, &me)).or(Some(String::new())),
                        ));
                    } else if row.clicked() {
                        pick = Some((h.call.clone(), None));
                    }
                    ui.add_space(3.0);
                }
            });

        if let Some((call, draft)) = pick {
            self.js8_select(&call, draft, &js8.heard);
        }
    }

    /// The conversation: every reassembled transmission, newest at the bottom.
    ///
    /// Rows are clickable — that is where a heartbeat, a CQ or a `HW CPY?`
    /// turns into the reply it expects.
    fn js8_conversation(&mut self, ui: &mut egui::Ui, js8: &sdroxide_types::Js8Status) {
        let me = self.js8_me(js8);
        let mut pick: Option<(String, Option<String>)> = None;
        egui::ScrollArea::vertical()
            .id_salt("js8-convo")
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .show_themed(ui, |ui| {
                if js8.messages.is_empty() {
                    ui.label(RichText::new("— no messages —").weak());
                }
                for (i, m) in js8.messages.iter().enumerate() {
                    let selected =
                        !m.from.is_empty() && self.js8_target.eq_ignore_ascii_case(&m.from);
                    let to_me = js8_personally_addressed(m);
                    let inner = egui::Frame::new()
                        .fill(if to_me { crate::theme::TOME_BG } else { crate::theme::ROW_BG })
                        .inner_margin(egui::Margin { left: 8, right: 5, top: 3, bottom: 3 })
                        .show(ui, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                ui.set_min_width(ui.available_width());
                                ui.spacing_mut().item_spacing.x = 4.0;
                                let (_, _, _, h, mi, _) =
                                    sdroxide_types::utc_ymd_hms(m.last_slot_utc);
                                ui.label(
                                    RichText::new(format!("{h:02}:{mi:02}")).monospace().weak(),
                                );
                                if to_me {
                                    ui.label(RichText::new("★").color(crate::theme::YELLOW));
                                }
                                let who = if m.from.is_empty() { "…" } else { &m.from };
                                ui.label(
                                    RichText::new(format!("{who}:")).monospace().strong().color(
                                        if to_me {
                                            crate::theme::CYAN
                                        } else {
                                            crate::theme::CYAN_DIM
                                        },
                                    ),
                                );
                                if let Some(c) = &m.cmd {
                                    ui.label(
                                        RichText::new(c).monospace().color(crate::theme::PINK),
                                    );
                                }
                                let body = RichText::new(&m.text).monospace();
                                // An incomplete message is still arriving; greying
                                // it stops a half-sentence reading as the whole one.
                                ui.label(if m.complete { body } else { body.weak() });
                                if !m.complete {
                                    ui.label(
                                        RichText::new(format!("… ({} frames)", m.frames)).weak(),
                                    );
                                }
                            });
                        });

                    let r = inner.response.rect;
                    ui.painter().rect_filled(
                        egui::Rect::from_min_max(
                            r.left_top(),
                            egui::pos2(r.left() + if to_me { 3.0 } else { 2.0 }, r.bottom()),
                        ),
                        0.0,
                        if to_me { crate::theme::YELLOW } else { crate::theme::CYAN_DIM },
                    );
                    let mut row = ui.interact(r, ui.id().with(("js8m", i)), egui::Sense::click());
                    // Drafted only for the row under the cursor: the log holds
                    // two hundred of these and every draft is an allocation.
                    if !m.from.is_empty() && (row.hovered() || row.clicked()) {
                        let draft = js8_reply_for(m, &me);
                        row = row.on_hover_text(match &draft {
                            Some(d) => format!("Reply to {}: “{d}”", m.from),
                            None => format!("Address the composer at {}", m.from),
                        });
                        if row.clicked() {
                            pick = Some((m.from.clone(), draft));
                        }
                    }
                    if selected || row.hovered() {
                        ui.painter().rect_stroke(
                            r,
                            0.0,
                            egui::Stroke::new(
                                1.0,
                                if selected {
                                    crate::theme::YELLOW
                                } else {
                                    crate::theme::CYAN_DIM
                                },
                            ),
                            egui::StrokeKind::Inside,
                        );
                    }
                    ui.add_space(2.0);
                }
            });
        if let Some((call, draft)) = pick {
            self.js8_select(&call, draft, &js8.heard);
        }
    }

    /// Facts about our own station the reply drafts may quote.
    fn js8_me(&self, js8: &sdroxide_types::Js8Status) -> Js8Me {
        let cfg = self.digi_status.as_ref().map(|s| &s.config).unwrap_or(&self.digi_cfg_edit);
        Js8Me {
            grid: cfg.my_grid.to_uppercase(),
            status: cfg.js8_status.clone(),
            hearing: js8.heard.iter().take(4).map(|h| h.call.clone()).collect(),
            last_sent: self.js8_last_sent.clone(),
        }
    }

    /// Aim the composer at a station, optionally with a draft in it, and put
    /// them on the map as the preview marker.
    fn js8_select(
        &mut self,
        call: &str,
        draft: Option<String>,
        heard: &[sdroxide_types::Js8Heard],
    ) {
        self.js8_target = call.to_string();
        if let Some(d) = draft {
            self.text_tx = d;
        }
        let ll = self.js8_grid_for(call, heard).as_deref().and_then(sdroxide_types::grid_to_latlon);
        self.digi_preview = ll.map(|ll| (call.to_string(), ll));
    }

    /// The two rows under the JS8 conversation: the actions, and the composer.
    ///
    /// **Declared bottom-first.** The caller lays this out with
    /// [`egui::Layout::bottom_up`] so the controls claim their true height and
    /// the conversation gets the remainder, which means the first row written
    /// here is the one that appears lowest.
    fn js8_compose(
        &mut self,
        ui: &mut egui::Ui,
        cmds: &mut Vec<Command>,
        js8: &sdroxide_types::Js8Status,
    ) {
        let has_target = !self.js8_target.is_empty();

        // Actions — the lower of the two rows. Wrapped, because the right
        // column can be dragged narrow and a chip that does not fit must move
        // to the next line rather than be clipped off the edge.
        ui.horizontal_wrapped(|ui| {
            if crate::chrome::chip(ui, false, " CQ ").clicked() {
                cmds.push(Command::DigiCallCq);
            }
            if crate::chrome::chip(ui, false, " HB ").clicked() {
                cmds.push(Command::DigiSendText("@ALLCALL HB".into()));
            }
            // The queries address whichever station is selected. Shown greyed
            // rather than hidden when there is none: a row that changes shape
            // as you click around is hard to aim at, and chips that only exist
            // sometimes are chips nobody discovers.
            ui.add_enabled_ui(has_target, |ui| {
                for q in ["SNR?", "GRID?", "HEARING?", "STATUS?", "HW CPY?"] {
                    if crate::chrome::chip(ui, false, q).clicked() {
                        let full = format!("{} {q}", self.js8_target);
                        self.js8_last_sent = full.clone();
                        cmds.push(Command::DigiSendText(full));
                    }
                }
                // The two that close a contact. Worth a button of their own:
                // they are the most-typed things on the band, and typing them
                // is the one moment an operator is not watching the panel.
                for q in ["RR", "73"] {
                    if crate::chrome::chip(ui, false, q).clicked() {
                        let full = format!("{} {q}", self.js8_target);
                        self.js8_last_sent = full.clone();
                        cmds.push(Command::DigiSendText(full));
                    }
                }
            });
            if has_target && crate::chrome::chip(ui, false, " CLEAR TO ").clicked() {
                self.js8_target.clear();
            }
        });

        // The gap between the two rows. In a bottom-up layout this space sits
        // above what was just written, so it separates the actions from the
        // composer rather than pushing them into the panel edge.
        ui.add_space(6.0);

        // Compose. The buttons are declared right-to-left first so they always
        // get their width, and the text box takes whatever is left over.
        let target = if has_target { self.js8_target.clone() } else { "@ALLCALL".to_string() };
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{target}:")).monospace().color(crate::theme::CYAN_DIM));
            let mut send = false;
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Stop lives next to send: they are the two things you reach
                // for in a hurry, and a long message takes minutes to drain.
                if crate::chrome::chip(ui, false, " STOP ").clicked() {
                    cmds.push(Command::DigiAbortTx);
                }
                send = crate::chrome::chip_accent(
                    ui,
                    false,
                    " SEND ",
                    crate::theme::PINK,
                    crate::theme::INK_ON_CYAN,
                )
                .clicked();
                // Before pressing send, say how long it will take. JS8's most
                // surprising property to a new operator is that a sentence can
                // occupy a minute of air time.
                if !self.text_tx.trim().is_empty() {
                    let frames = js8_frame_estimate(&self.text_tx);
                    ui.label(
                        RichText::new(format!(
                            "{frames}f · {:.0}s",
                            f64::from(frames) * js8.speed.slot_s()
                        ))
                        .monospace()
                        .weak(),
                    );
                }
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.text_tx)
                        .desired_width(ui.available_width().max(60.0))
                        .hint_text("Message…"),
                );
                send |= resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            });
            if send && !self.text_tx.trim().is_empty() {
                let body = self.text_tx.trim();
                let full = if has_target {
                    format!("{} {body}", self.js8_target)
                } else {
                    body.to_string()
                };
                // Kept so `AGN?` — "say again" — has something to draft from.
                self.js8_last_sent = full.clone();
                cmds.push(Command::DigiSendText(full));
                self.text_tx.clear();
            }
        });
    }

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
            self.digi_freq_chip(ui, cmds);
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
                                    for m in messages.iter().filter(|m| m.to_me && !m.to.is_empty())
                                    {
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
                                        RichText::new(&text_rx)
                                            .monospace()
                                            .color(crate::theme::GREEN),
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
                            if ui
                                .add(
                                    egui::TextEdit::singleline(&mut c.name)
                                        .hint_text("name")
                                        .desired_width(120.0),
                                )
                                .changed()
                            {
                                changed = true;
                            }
                            if crate::chrome::chip_accent(
                                ui,
                                false,
                                "DEL",
                                crate::theme::PINK,
                                crate::theme::INK_ON_CYAN,
                            )
                            .clicked()
                            {
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

    /// Hellschreiber variant chips, shown inline in the Hell panel header next
    /// to the tune buttons.
    ///
    /// The variant lives in `DigiConfig` (the engine has to know the dot rate);
    /// contrast and reverse video live in `ViewState`, so moving them repaints
    /// the whole scrollback rather than just what arrives next.
    fn hell_params_row(&mut self, ui: &mut egui::Ui, cmds: &mut Vec<Command>) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            ui.label(RichText::new("Mode").size(10.5).strong().color(crate::theme::CYAN_DIM));
            let cfg = &mut self.digi_cfg_edit;
            let mut changed = false;
            for v in sdroxide_types::HellVariant::ALL {
                let sel = cfg.hell_variant == v;
                let hint = format!(
                    "{} — {:.1} char/s, {:.0} Hz wide, {}",
                    v.label(),
                    v.chars_per_sec(),
                    v.bandwidth_hz(),
                    if v.is_fsk() { "frequency-shifted" } else { "on/off keyed" }
                );
                if ui.selectable_label(sel, v.label()).on_hover_text(hint).clicked() && !sel {
                    cfg.hell_variant = v;
                    changed = true;
                }
            }
            if changed {
                cmds.push(Command::SetDigiConfig(self.digi_cfg_edit.clone()));
            }
        });
    }

    /// Own-call / grid / message-template editor (and RTTY parameters).
    fn digi_settings_window(&mut self, ctx: &egui::Context, cmds: &mut Vec<Command>) {
        let mut open = self.show_digi_settings;
        let mode = self.state.rx[0].mode;
        // Per-mode parameters (RTTY/Olivia/THOR/FSQ) now live in each panel's
        // header, so this dialog only carries the shared identity + FT8/FT4
        // message templates.
        let title = if mode.is_text_modem() || mode.is_hell() || mode.is_js8() {
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
                    if mode.is_js8() {
                        let turbo = cfg.js8_speed == sdroxide_types::Js8Speed::Turbo;
                        ui.label("Auto-reply");
                        changed |= ui
                            .checkbox(&mut cfg.js8_auto_reply, "Answer SNR? / GRID? / STATUS?")
                            .on_hover_text(
                                "Answer a direct question addressed to you or to @ALLCALL, with \
                                 the answer rather than an acknowledgement. Never answers another \
                                 station's traffic, and never answers itself.",
                            )
                            .changed();
                        ui.end_row();
                        ui.label("Heartbeat");
                        ui.horizontal(|ui| {
                            // The intervals JS8Call offers, plus off. A beacon
                            // is a commitment of air time, so the choice is a
                            // few sensible ones rather than a free number.
                            for (mins, label) in [
                                (0u32, "Off"),
                                (10, "10 min"),
                                (15, "15 min"),
                                (30, "30 min"),
                                (60, "60 min"),
                            ] {
                                if crate::chrome::chip(ui, cfg.js8_heartbeat_min == mins, label)
                                    .clicked()
                                    && cfg.js8_heartbeat_min != mins
                                {
                                    cfg.js8_heartbeat_min = mins;
                                    changed = true;
                                }
                            }
                        });
                        ui.end_row();
                        ui.label("");
                        // Off by default and worth saying why: a beacon that
                        // switches itself on is an on-air behaviour the
                        // operator never chose.
                        ui.label(
                            RichText::new(if turbo {
                                "Turbo does not beacon — it is the local and VHF speed."
                            } else {
                                "Sends your callsign and grid so others know you are receivable. \
                                 The first goes out one interval from now, not immediately."
                            })
                            .size(10.5)
                            .weak(),
                        );
                        ui.end_row();
                        ui.label("Beacon frequency");
                        ui.horizontal(|ui| {
                            let sub_band = !cfg.js8_hb_anywhere;
                            if crate::chrome::chip(ui, sub_band, "500–1000 Hz")
                                .on_hover_text(
                                    "Move each beacon to a free slot in the heartbeat sub-band, \
                                     the way JS8Call does: it is where stations watching for \
                                     beacons look, and it keeps an unattended transmitter off \
                                     somebody else's QSO. The slot is chosen when the beacon \
                                     actually goes out, clear of everything being decoded.",
                                )
                                .clicked()
                                && !sub_band
                            {
                                cfg.js8_hb_anywhere = false;
                                changed = true;
                            }
                            if crate::chrome::chip(ui, !sub_band, "Working freq")
                                .on_hover_text(
                                    "Beacon where you are working instead. Against the band \
                                     convention, but it keeps everything you transmit in one \
                                     place.",
                                )
                                .clicked()
                                && sub_band
                            {
                                cfg.js8_hb_anywhere = true;
                                changed = true;
                            }
                        });
                        ui.end_row();
                        ui.label("Heartbeat reply");
                        ui.add_enabled_ui(cfg.js8_auto_reply && !turbo, |ui| {
                            changed |= ui
                                .checkbox(
                                    &mut cfg.js8_hb_ack,
                                    "Answer heartbeats with a signal report",
                                )
                                .on_hover_text(
                                    "Tell a station that beaconed how well you copied them. Off \
                                     by default: a busy band carries a heartbeat every slot, and \
                                     answering all of them would flood exactly the band \
                                     heartbeats exist to keep quiet. Rate-limited to one answer \
                                     per station every 15 minutes, and never while a message is \
                                     still arriving or while you have something queued to send.",
                                )
                                .changed();
                        });
                        ui.end_row();
                        ui.label("Status message");
                        changed |= ui.text_edit_singleline(&mut cfg.js8_status).changed();
                        ui.end_row();
                    }
                    ui.label("TX period");
                    ui.horizontal(|ui| {
                        changed |= ui.selectable_value(&mut cfg.tx_even, true, "Even").changed();
                        changed |= ui.selectable_value(&mut cfg.tx_even, false, "Odd").changed();
                    });
                    ui.end_row();
                    ui.label("Auto-sequence");
                    changed |= ui.checkbox(&mut cfg.auto_seq, "").changed();
                    ui.end_row();
                    ui.label("Auto TX frequency");
                    changed |= ui
                        .checkbox(&mut cfg.auto_tx_freq, "")
                        .on_hover_text(
                            "Choose the transmit frequency automatically: the quietest spot in \
                             the period you transmit in, rather than the frequency of the \
                             station you are answering. Off holds whatever you set by hand.",
                        )
                        .changed();
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
                    ui.label("DXpedition");
                    ui.horizontal(|ui| {
                        for m in sdroxide_types::DxpedMode::ALL {
                            changed |= ui
                                .selectable_value(&mut cfg.dxped_mode, m, m.label())
                                .on_hover_text(match m {
                                    sdroxide_types::DxpedMode::Normal => "Ordinary FT8 operation.",
                                    sdroxide_types::DxpedMode::Hound => {
                                        "Calling a DXpedition running Fox mode: call from above \
                                         1000 Hz, move down onto the Fox when it answers, and \
                                         log on its RR73 without sending 73."
                                    }
                                    sdroxide_types::DxpedMode::Fox => {
                                        "Run the pile-up: several signals at once, a queue of \
                                         callers, worked strongest and rarest first. CALL CQ \
                                         starts it, STOP QSO stands it down."
                                    }
                                })
                                .changed();
                        }
                    });
                    ui.end_row();
                    if cfg.dxped_mode == sdroxide_types::DxpedMode::Fox {
                        ui.label("Fox signals");
                        changed |= ui
                            .add(
                                egui::DragValue::new(&mut cfg.fox_slots)
                                    .range(1..=sdroxide_types::FOX_MAX_SLOTS)
                                    .suffix(" at once"),
                            )
                            .on_hover_text(
                                "Simultaneous transmissions, spaced 60 Hz apart. They share the \
                                 transmitter's power, so more signals means each is weaker.",
                            )
                            .changed();
                        ui.end_row();
                    }
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
                egui::Grid::new("voice-grid")
                    .num_columns(6)
                    .spacing([8.0, 6.0])
                    .striped(true)
                    .show(ui, |ui| {
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
                                cmds.push(Command::VoiceRecord(if is_rec {
                                    None
                                } else {
                                    Some(i as u8)
                                }));
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
                                let erasable = !slot.is_empty() && !is_rec && !is_play && !is_prev;
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
                    });

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

    /// The live-spots window: source filters, a fuzzy search box, a
    /// click-to-tune list of current DX-cluster / POTA / SOTA / PSK-Reporter
    /// spots and broadcast stations, and the feed status line.
    fn spots_window(&mut self, ctx: &egui::Context, cmds: &mut Vec<Command>) {
        let worked_entities = self.worked_entities().clone();
        let mut open = self.show_spots;
        let mut clicked: Option<Spot> = None;
        let mut open_setup = false;
        let now = now_unix();
        self.refresh_broadcast_spots(now);
        // Cloned out of `self` because the window closure needs `&mut self`.
        let spots = self.merged_spots();
        // Chip order has to match `spot_kind_index`: the loop below indexes
        // `spot_kinds_shown` positionally.
        let labels = [
            (SpotKind::DxCluster, "DX"),
            (SpotKind::Pota, "POTA"),
            (SpotKind::Sota, "SOTA"),
            (SpotKind::PskReporter, "PSK"),
            (SpotKind::FreeDv, "FREEDV"),
            (SpotKind::Broadcast, "BC"),
        ];
        let resp = egui::Window::new("SPOTS")
            .open(&mut open)
            .frame(crate::chrome::window_frame())
            .resizable(true)
            .default_width(580.0)
            .default_height(480.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    for (i, (kind, label)) in labels.iter().enumerate() {
                        let chip = crate::chrome::chip(ui, self.spot_kinds_shown[i], *label);
                        let chip = if *kind == SpotKind::Broadcast {
                            chip.on_hover_text(
                                "Longwave & shortwave broadcast stations on air now",
                            )
                        } else {
                            chip
                        };
                        if chip.clicked() {
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
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 5.0;
                    ui.label(RichText::new("⌕").color(crate::theme::CYAN_DIM).size(14.0));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.spot_search)
                            .desired_width(200.0)
                            .hint_text("call, station, site, frequency")
                            .text_color(crate::theme::TEXT_STRONG),
                    );
                    if !self.spot_search.trim().is_empty()
                        && ui.button("✕").on_hover_text("Clear the search").clicked()
                    {
                        self.spot_search.clear();
                    }
                });
                if let Some(s) = &self.net_status {
                    ui.label(RichText::new(s).size(11.0).color(Color32::from_gray(150)));
                }
                ui.separator();
                // Filter by the category chips, then rank by how well each row
                // matched the query. With no query the natural frequency order
                // is kept; with one, the best matches come first, because the
                // whole point of typing is to get the wanted row to the top.
                let query = self.spot_search.trim();
                let visible: Vec<&Spot> = spots.iter().filter(|s| self.spot_visible(s)).collect();
                let mut rows: Vec<(&Spot, i32)> = visible
                    .iter()
                    .filter_map(|s| {
                        crate::fuzzy::score_terms(&spot_haystack(s), query).map(|sc| (*s, sc))
                    })
                    .collect();
                if !query.is_empty() {
                    rows.sort_by_key(|r| std::cmp::Reverse(r.1));
                    // Counted against what the chips let through, not against
                    // every spot held — "3 of 5" when three categories are off
                    // would look like the search had lost the rest.
                    let (text, colour) = match rows.len() {
                        0 => ("no match".to_string(), crate::theme::PINK),
                        n => (format!("{n} of {}", visible.len()), crate::theme::YELLOW),
                    };
                    ui.label(RichText::new(text).color(colour).size(10.0));
                }
                egui::ScrollArea::vertical().auto_shrink([false, false]).show_themed(ui, |ui| {
                    for (s, _) in &rows {
                        let needed = s.kind != SpotKind::Broadcast
                            && sdroxide_types::entity_name(&s.call)
                                .map(|n| !worked_entities.contains(n))
                                .unwrap_or(false);
                        if spot_row(ui, s, now, needed).clicked() {
                            clicked = Some((*s).clone());
                        }
                    }
                    if rows.is_empty() {
                        ui.add_space(8.0);
                        let msg = if query.is_empty() {
                            "no spots — enable a feed in ⚙ SETUP"
                        } else {
                            "nothing matches the search"
                        };
                        ui.label(RichText::new(msg).color(Color32::from_gray(120)));
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
                            crate::download::load_text(
                                "ADIF",
                                "adi",
                                self.adif_import_inbox.clone(),
                            );
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
            let band = if freq_hz > 0.0 {
                sdroxide_types::adif_band(freq_hz).to_string()
            } else {
                String::new()
            };
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
                        ui.new_child(
                            egui::UiBuilder::new()
                                .max_rect(rect)
                                .layout(egui::Layout::left_to_right(egui::Align::Center)),
                        )
                        .label(text);
                    };
                    let field = |ui: &mut egui::Ui, w: f32, s: &mut String| {
                        ui.add(egui::TextEdit::singleline(s).desired_width(w));
                    };
                    ui.horizontal(|ui| {
                        lbl(ui, "Call");
                        let cr =
                            ui.add(egui::TextEdit::singleline(&mut f.call).desired_width(150.0));
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
                            // A hand-entered contact is one worked this session
                            // too; an ADIF import is not, and does not count.
                            self.session_qsos += 1;
                        } else if let Some(e) = self.qso_log.iter_mut().find(|q| q.id == rec.id) {
                            // An edit can change the call, the grid or the QSL
                            // flags, none of which move the log's length.
                            *e = rec;
                            self.log_content_changed();
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
                                    egui::UiBuilder::new()
                                        .max_rect(rect)
                                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                                );
                                c.add(lbl);
                            };
                            let gray = Color32::from_gray(150);
                            col(
                                ui,
                                40.0,
                                egui::Label::new(
                                    RichText::new(time_str(r.start_utc))
                                        .monospace()
                                        .size(12.0)
                                        .color(gray),
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
                                egui::Label::new(
                                    RichText::new(rst).monospace().size(11.5).color(gray),
                                ),
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
                                let mut c = ui.new_child(
                                    egui::UiBuilder::new()
                                        .max_rect(rect)
                                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                                );
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
                                    if crate::chrome::chip(
                                        ui,
                                        false,
                                        RichText::new("EDIT").size(11.0),
                                    )
                                    .clicked()
                                    {
                                        to_edit = Some(r.id);
                                    }
                                    if !up_targets.is_empty()
                                        && crate::chrome::chip(
                                            ui,
                                            false,
                                            RichText::new("UP").size(11.0),
                                        )
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
                    egui::Rect::from_min_max(
                        rr.left_top(),
                        egui::pos2(rr.left() + 2.0, rr.bottom()),
                    ),
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
            // The satellite config is the client's own, so it comes from the
            // live copy rather than from the engine. Subscription status is
            // read from the disk cache, which is the only source that has an
            // answer when the solar window has never been opened.
            self.sat_cfg_edit = (*self.sat_cfg).clone();
            self.refresh_sat_sub_status();
            self.audio_devices_queried = true;
        }
        // Edits collected here and applied after the window closure, which
        // borrows `&self` and so can't touch `&mut self.ctrl`.
        let mut audio_pick: Option<(bool, Option<String>)> = None;
        let mut hpsdr_discover = false;
        let mut rtlsdr_rescan = false;
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
        let mut sat_edit = self.sat_cfg_edit.clone();
        let mut sat_ui = std::mem::take(&mut self.sat_ui);
        let mut sat_sub_refresh = false;
        let sat_subs = self.sat_sub_views();
        let mut bc_reload = false;
        let mut bc_restore = false;

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
        // Ungated, unlike SoapySDR: the RTL-SDR driver is pure Rust and needs
        // no system library, so it is compiled into every build variant.
        iface_opts.push(sdroxide_types::Backend::RtlSdr);
        iface_opts.push(sdroxide_types::Backend::Flex);
        iface_opts.push(sdroxide_types::Backend::Icom);

        let mut tab = self.settings_tab;
        let mut open = self.show_settings;
        // The 3D window owns the live copy of its own settings — `view.solar3d`
        // is only the snapshot persisted from it — so this is read out of the
        // window here and handed back to it below, the way `ui_edit` is.
        #[cfg(not(target_arch = "wasm32"))]
        let mut solar_cloud_march = self.solar.cloud_march();
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
                        rtlsdr_rescan: &mut rtlsdr_rescan,
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
                        bc_reload: &mut bc_reload,
                        bc_restore: &mut bc_restore,
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
                        sat_edit: &mut sat_edit,
                        sat_ui: &mut sat_ui,
                        sat_subs: &sat_subs,
                        sat_sub_refresh: &mut sat_sub_refresh,
                        #[cfg(not(target_arch = "wasm32"))]
                        solar_cloud_march: Some(&mut solar_cloud_march),
                        #[cfg(target_arch = "wasm32")]
                        solar_cloud_march: None,
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
        #[cfg(not(target_arch = "wasm32"))]
        if solar_cloud_march != self.solar.cloud_march() {
            self.solar.set_cloud_march(solar_cloud_march);
        }
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
        self.sat_ui = sat_ui;
        if sat_edit != self.sat_cfg_edit {
            // Written straight out, like the input bindings: there is no APPLY
            // step here, and a satellite the operator cannot see saved is one
            // they will add again after the next restart. The solar window
            // picks the new `Arc` up on its next frame.
            self.sat_cfg_edit = sat_edit;
            self.sat_cfg_edit.prune();
            self.sat_cfg = std::sync::Arc::new(self.sat_cfg_edit.clone());
            persist_sat_config(&self.sat_cfg_edit);
        }
        if sat_sub_refresh {
            // Blocking: one HTTPS round trip per subscription. After the window
            // closure, the way the HPSDR scan is.
            self.refresh_sat_subs_now();
        }
        if bc_restore {
            restore_bundled_broadcast_stations();
        }
        if bc_reload || bc_restore {
            self.broadcast = load_broadcast_stations();
            // Force a rebuild rather than waiting up to a minute for the tick.
            self.broadcast_minute = -1;
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
        if rtlsdr_rescan {
            // USB enumeration only — no device is opened, so this is safe to
            // press at any time, including while a dongle is streaming.
            self.rtlsdr_devices = self.ctrl.list_rtlsdr();
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
                (SettingsTab::Tle, "TLE"),
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
                    Backend::Hpsdr => settings_hpsdr_tab(
                        ui,
                        &self.hpsdr_devices,
                        io.radio_edit,
                        io.hpsdr_discover,
                        cmds,
                    ),
                    Backend::Cat => settings_cat_tab(ui, &self.serial_ports, io.radio_edit),
                    Backend::Tci => {
                        settings_tci_tab(ui, io.radio_edit, io.tci_test, &self.tci_test_result)
                    }
                    Backend::RtlSdr => settings_rtlsdr_tab(
                        ui,
                        &self.rtlsdr_devices,
                        io.radio_edit,
                        io.rtlsdr_rescan,
                        cmds,
                    ),
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
                    ui.label(RichText::new("Switches the live radio without restarting.").weak());
                });
            }
            SettingsTab::Ui => settings_ui_tab(ui, io.ui_edit, io.solar_cloud_march.as_deref_mut()),
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

                net_heading(ui, "Broadcast stations");
                broadcast_stations_settings(ui, io.bc_reload, io.bc_restore);
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
            SettingsTab::Tle => settings_tle_tab(ui, io),
        }
    }

    /// Subscription status for the settings dialog.
    ///
    /// The live feed is preferred — it has the result of the fetch it just did
    /// — but it only exists while the solar window is open, so the disk cache
    /// answers for the far more common case of the dialog being opened with the
    /// window shut.
    #[cfg(not(target_arch = "wasm32"))]
    fn refresh_sat_sub_status(&mut self) {
        let live = self.solar.tle_sub_status();
        let subs: Vec<_> = self.sat_cfg.subs.clone();
        self.sat_sub_status =
            if live.is_empty() { sdroxide_solar::tlesub::status_all(&subs) } else { live }
                .into_iter()
                .map(|s| SubStatusView {
                    url: s.url,
                    fetched_unix: s.fetched_unix,
                    count: s.count,
                    curated: s.curated,
                    error: s.error,
                })
                .collect();
    }

    #[cfg(target_arch = "wasm32")]
    fn refresh_sat_sub_status(&mut self) {}

    fn sat_sub_views(&self) -> Vec<SubStatusView> {
        self.sat_sub_status.clone()
    }

    /// Fetch every enabled subscription now, from the settings dialog's UPDATE
    /// NOW button. Blocking — up to one HTTPS round trip per subscription.
    ///
    /// The solar window's feed shares the same disk cache, so a listing fetched
    /// here is what it serves next time it looks, without a second request.
    #[cfg(not(target_arch = "wasm32"))]
    fn refresh_sat_subs_now(&mut self) {
        let subs: Vec<_> = self.sat_cfg_edit.subs.clone();
        let done = sdroxide_solar::tlesub::refresh_all(&subs);
        let failed = done.iter().filter(|s| s.error.is_some()).count();
        let total: usize = done.iter().map(|s| s.count).sum();
        self.sat_ui.note = match (done.len(), failed) {
            (0, _) => "No enabled subscriptions to update.".to_string(),
            (n, 0) => format!("Updated {n} subscription(s): {total} satellites."),
            (n, f) => format!("Updated {} of {n}; {f} failed — see the rows above.", n - f),
        };
        self.refresh_sat_sub_status();
        // The window's feed is told to re-read the cache rather than being left
        // on what it loaded at open time.
        self.solar.reload_tle_subs();
    }

    #[cfg(target_arch = "wasm32")]
    fn refresh_sat_subs_now(&mut self) {}

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
            ComboBox::from_id_salt("ant-rx").selected_text(self.state.antenna_rx.clone()).show_ui(
                ui,
                |ui| {
                    for a in &caps.antennas_rx {
                        if ui.selectable_label(self.state.antenna_rx == *a, a).clicked() {
                            cmds.push(Command::SetAntenna { dir: Direction::Rx, name: a.clone() });
                        }
                    }
                },
            );
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

fn settings_ui_tab(
    ui: &mut egui::Ui,
    cfg: &mut sdroxide_types::UiSettings,
    cloud_march: Option<&mut bool>,
) {
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

    let Some(cloud_march) = cloud_march else { return };
    ui.add_space(14.0);
    ui.label(RichText::new("3D view").size(14.0).strong().color(crate::theme::CYAN));
    ui.add_space(6.0);
    egui::Grid::new("ui-grid-3d").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
        ui.label("Cloud rendering");
        ComboBox::from_id_salt("ui-cloud-march")
            .selected_text(if *cloud_march { "Volumetric" } else { "Layered" })
            .show_ui(ui, |ui| {
                ui.selectable_value(cloud_march, false, "Layered");
                ui.selectable_value(cloud_march, true, "Volumetric");
            });
        ui.end_row();
    });
    ui.add_space(8.0);
    ui.label(
        RichText::new(
            "How the CLOUDS layer in the 3D view draws the weather. Layered stacks \
             slices through the troposphere and is the cheap option. Volumetric walks \
             a ray through it instead, so the Sun casts the cloud tops onto the deck \
             below and lightning glows out through the storm making it rather than \
             only brightening its outside — at several times the cost per pixel.",
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
    use sdroxide_types::{
        CatFamily, DigiMode, LineState, ModeControl, Parity, PttMethod, SoundFormat, StopBits,
    };
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
            ui.add(
                DragValue::new(&mut cfg.cat.audio_bw_hz)
                    .speed(100.0)
                    .range(1000.0..=24000.0)
                    .suffix(" Hz"),
            );
            ui.end_row();
        }

        ui.label("Serial port");
        let shown = if cfg.cat.serial.path.is_empty() {
            "— select —".to_string()
        } else {
            cfg.cat.serial.path.clone()
        };
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
        ComboBox::from_id_salt("baud").selected_text(cfg.cat.serial.baud.to_string()).show_ui(
            ui,
            |ui| {
                for b in [4800u32, 9600, 19200, 38400, 57600, 115200] {
                    if ui.selectable_label(cfg.cat.serial.baud == b, b.to_string()).clicked() {
                        cfg.cat.serial.baud = b;
                    }
                }
            },
        );
        ui.end_row();

        ui.label("Data bits");
        ComboBox::from_id_salt("databits")
            .selected_text(cfg.cat.serial.data_bits.to_string())
            .show_ui(ui, |ui| {
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
    cmds: &mut Vec<Command>,
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
            ComboBox::from_id_salt("hpsdr_dev").width(320.0).selected_text(shown).show_ui(
                ui,
                |ui| {
                    if devices.is_empty() {
                        ui.label(RichText::new("no devices — press Discover").weak());
                    }
                    for d in devices {
                        // Both protocols are drivable; anything else is greyed out.
                        if d.supported() {
                            let sel = cfg.hpsdr.selected_ip.as_deref() == Some(d.ip.as_str());
                            if ui.selectable_label(sel, d.label()).clicked() {
                                cfg.hpsdr.selected_ip = Some(d.ip.clone());
                            }
                        } else {
                            ui.label(RichText::new(d.label()).weak());
                        }
                    }
                },
            );
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

        ui.label("LNA gain").on_hover_text(
            "Front-end gain of a Hermes-Lite 2. Takes effect immediately — no reconnect — \
             and is remembered as the level the radio starts at. Too high clips the ADC and \
             the whole band looks distorted; too low and the receiver goes deaf.",
        );
        // Applies live as well as being persisted: this is the gain an operator
        // retunes per band, and making it wait for Apply/reconnect would mean
        // dropping the stream every time they nudge it.
        if crate::chrome::slider(
            ui,
            Slider::new(
                &mut cfg.hpsdr.lna_gain_db,
                HpsdrConfig::LNA_GAIN_MIN_DB..=HpsdrConfig::LNA_GAIN_MAX_DB,
            )
            .step_by(1.0)
            .suffix(" dB"),
        )
        .changed()
        {
            cmds.push(Command::SetGain {
                dir: Direction::Rx,
                element: sdroxide_types::HpsdrConfig::LNA_GAIN_ELEMENT.to_string(),
                db: cfg.hpsdr.lna_gain_db,
            });
        }
        ui.end_row();

        ui.label("Filter board").on_hover_text(
            "Accessory board on the Hermes-Lite 2's J16 header. Leave this at \"None\" \
             unless a filter board is actually fitted: those seven pins are \
             general-purpose open-collector outputs, and operators also wire them to \
             amplifier PTT, antenna relays and transverter switching. Driving them from \
             band data would start operating whatever is connected.",
        );
        ComboBox::from_id_salt("hpsdr_filter")
            .width(220.0)
            .selected_text(cfg.hpsdr.filter_board.label())
            .show_ui(ui, |ui| {
                for b in sdroxide_types::HpsdrFilterBoard::ALL {
                    if ui.selectable_label(cfg.hpsdr.filter_board == b, b.label()).clicked() {
                        cfg.hpsdr.filter_board = b;
                    }
                }
            });
        ui.end_row();

        ui.label("Invert spectrum");
        ui.checkbox(&mut cfg.hpsdr.invert_spectrum, "Swap I/Q").on_hover_text(
            "Mirror the board's spectrum about the tuned frequency, on transmit as well \
             as receive. On by default: a Hermes-Lite 2 needs it. Turn it off only if \
             signals show up on the wrong side of the dial and nothing decodes — the \
             giveaway is a waterfall full of convincing traces while SSB lands on the \
             wrong sideband and FT8 returns no decodes at all.",
        );
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

/// RTL-SDR interface: which dongle, sample rate, gain/AGC, frequency
/// correction, HF reception and the bias tee.
///
/// Gain, AGC, ppm and the bias tee all apply *live* rather than waiting for
/// Apply/reconnect — these are the controls an operator moves while listening,
/// and dropping the stream on every nudge would make them unusable. The dongle
/// selection and sample rate do need a reconnect, since both are fixed when
/// the device is opened.
fn settings_rtlsdr_tab(
    ui: &mut egui::Ui,
    devices: &[sdroxide_types::RtlSdrDevice],
    radio_edit: &mut Option<sdroxide_types::RadioConfig>,
    rescan: &mut bool,
    cmds: &mut Vec<Command>,
) {
    use sdroxide_types::{RtlSdrAgc, RtlSdrConfig, RtlSdrHfMode};
    let Some(cfg) = radio_edit.as_mut() else {
        ui.label("Radio configuration is only available in the native app.");
        return;
    };

    egui::Grid::new("rtlsdr-grid").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
        ui.label("Dongle");
        ui.horizontal(|ui| {
            if ui
                .button("Rescan")
                .on_hover_text(
                    "Re-list the USB bus. No device is opened, so this is safe \
                     to press while receiving.",
                )
                .clicked()
            {
                *rescan = true;
            }
            let shown = cfg.rtlsdr.serial.clone().unwrap_or_else(|| "— first one found —".into());
            ComboBox::from_id_salt("rtlsdr_dev").width(300.0).selected_text(shown).show_ui(
                ui,
                |ui| {
                    if devices.is_empty() {
                        ui.label(RichText::new("no dongles — press Rescan").weak());
                    }
                    if ui
                        .selectable_label(cfg.rtlsdr.serial.is_none(), "— first one found —")
                        .clicked()
                    {
                        cfg.rtlsdr.serial = None;
                    }
                    for d in devices {
                        // Only a dongle with a serial can be pinned; without
                        // one there is nothing stable to remember, since bus
                        // position changes on every replug.
                        if let Some(sn) = &d.serial {
                            let sel = cfg.rtlsdr.serial.as_deref() == Some(sn.as_str());
                            if ui.selectable_label(sel, d.label()).clicked() {
                                cfg.rtlsdr.serial = Some(sn.clone());
                            }
                        } else {
                            ui.label(RichText::new(d.label()).weak());
                        }
                    }
                },
            );
        });
        ui.end_row();

        ui.label("Sample rate").on_hover_text(
            "The RTL2832U's resampler reaches 225–300 kHz and 900 kHz–3.2 MHz, \
             nothing between. Takes effect on Apply.",
        );
        let shown = format!("{:.3} Msps", cfg.rtlsdr.sample_rate_hz / 1e6);
        ComboBox::from_id_salt("rtlsdr_rate").selected_text(shown).show_ui(ui, |ui| {
            for &r in &RtlSdrConfig::SAMPLE_RATES {
                let sel = (cfg.rtlsdr.sample_rate_hz - r).abs() < 1.0;
                let mut label = format!("{:.3} Msps", r / 1e6);
                if r >= 3_200_000.0 {
                    label.push_str("  (often drops samples)");
                }
                if ui.selectable_label(sel, label).clicked() {
                    cfg.rtlsdr.sample_rate_hz = r;
                }
            }
        });
        ui.end_row();

        ui.label("AGC").on_hover_text(
            "Manual is the setting for measurement and weak-signal digital modes. \
             The tuner and the demodulator have independent automatic loops.",
        );
        let mut agc = cfg.rtlsdr.agc;
        enum_combo(ui, "rtlsdr_agc", &mut agc, &RtlSdrAgc::ALL, RtlSdrAgc::label);
        if agc != cfg.rtlsdr.agc {
            cfg.rtlsdr.agc = agc;
            cmds.push(Command::SetGain {
                dir: Direction::Rx,
                element: RtlSdrConfig::AGC_ELEMENT.to_string(),
                db: agc.code() as f64,
            });
        }
        ui.end_row();

        ui.label("Tuner gain").on_hover_text(
            "Applies immediately — no reconnect. The tuner has 29 discrete steps, \
             so the value snaps to the nearest one it can produce. Ignored while \
             the tuner AGC is running.",
        );
        ui.add_enabled_ui(!cfg.rtlsdr.agc.tuner_auto(), |ui| {
            if crate::chrome::slider(
                ui,
                Slider::new(&mut cfg.rtlsdr.tuner_gain_db, 0.0..=RtlSdrConfig::GAIN_MAX_DB)
                    .step_by(0.1)
                    .suffix(" dB"),
            )
            .changed()
            {
                cmds.push(Command::SetGain {
                    dir: Direction::Rx,
                    element: RtlSdrConfig::TUNER_GAIN_ELEMENT.to_string(),
                    db: cfg.rtlsdr.tuner_gain_db,
                });
            }
        });
        ui.end_row();

        ui.label("Frequency correction").on_hover_text(
            "Crystal error in parts per million. Run with \
             RUST_LOG=sdroxide_rtlsdr=debug and the log prints the measured \
             clock error after about 20 seconds — that is the number to enter. \
             Applies immediately.",
        );
        let mut ppm = cfg.rtlsdr.ppm;
        if ui.add(egui::DragValue::new(&mut ppm).range(-200..=200).suffix(" ppm")).changed() {
            cfg.rtlsdr.ppm = ppm;
            cmds.push(Command::SetGain {
                dir: Direction::Rx,
                element: RtlSdrConfig::PPM_ELEMENT.to_string(),
                db: ppm as f64,
            });
        }
        ui.end_row();

        ui.label("HF reception").on_hover_text(
            "The tuner itself starts at 24 MHz. An RTL-SDR Blog V4 upconverts \
             below that in hardware; other dongles reach HF only by sampling the \
             ADC directly, through the V3's HF port. Switching modes briefly \
             interrupts the stream.",
        );
        let mut hf = cfg.rtlsdr.hf_mode;
        enum_combo(ui, "rtlsdr_hf", &mut hf, &RtlSdrHfMode::ALL, RtlSdrHfMode::label);
        if hf != cfg.rtlsdr.hf_mode {
            cfg.rtlsdr.hf_mode = hf;
            cmds.push(Command::SetGain {
                dir: Direction::Rx,
                element: RtlSdrConfig::HF_MODE_ELEMENT.to_string(),
                db: hf as u8 as f64,
            });
        }
        ui.end_row();

        ui.label("Bias tee");
        let mut bias = cfg.rtlsdr.bias_tee;
        if ui.checkbox(&mut bias, "Feed ~4.5 V DC up the coax").changed() {
            cfg.rtlsdr.bias_tee = bias;
            cmds.push(Command::SetGain {
                dir: Direction::Rx,
                element: RtlSdrConfig::BIAS_TEE_ELEMENT.to_string(),
                db: if bias { 1.0 } else { 0.0 },
            });
        }
        ui.end_row();
    });

    if cfg.rtlsdr.bias_tee {
        ui.add_space(4.0);
        ui.label(
            RichText::new(
                "Bias tee is ON. Never connect a transceiver, a grounded antenna, \
                 or a preamp powered from elsewhere while this is enabled — the DC \
                 goes straight down the feedline.",
            )
            .color(crate::theme::YELLOW),
        );
    }

    ui.add_space(4.0);
    ui.label(
        RichText::new(
            "Receive only. The dongle and sample rate take effect on Apply; \
             everything else applies as you change it.",
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
            ui.label(
                RichText::new(format!("Connected: {s}")).color(Color32::from_rgb(90, 200, 110)),
            );
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
fn operator_identity_note(ui: &mut egui::Ui, digi: &sdroxide_types::DigiConfig, seeded: bool) {
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
        ui.checkbox(&mut c.report_rx, "Report stations I decode").on_hover_text(
            "Sends an rx_report for each callsign recovered from a RADE \
                            End-of-Over frame.",
        );
        ui.checkbox(&mut c.show_spots, "Show other reporter stations as spots").on_hover_text(
            "Adds them to the panadapter overlay, world map and SPOTS window \
                            under the FREEDV filter.",
        );
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
        let all =
            Action::all().into_iter().chain(memories.iter().map(|m| Action::MemoryRecall(m.id)));
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
    egui::Grid::new("keys-grid").num_columns(6).spacing([10.0, 6.0]).striped(true).show(ui, |ui| {
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
    });
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
                .on_hover_text(
                    "Hold Space to transmit; releasing it — or losing window focus — unkeys",
                )
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
    .on_hover_text(
        "Backstop against a stuck key or a controller that stops reporting. 0 disables.",
    );

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
                ComboBox::from_id_salt(("mb", i))
                    .width(130.0)
                    .selected_text(b.button.label())
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
        RichText::new("F1 always opens this manual, even while typing, so it is not rebindable.")
            .weak(),
    );

    ui.add_space(10.0);
    ui.separator();
    ui.add_space(6.0);
    settings_midi_section(
        ui,
        cfg,
        io.midi_learn,
        io.midi_rescan,
        memories,
        midi_in,
        midi_out,
        midi_status,
        last_midi,
    );
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
        if !cfg.midi.bindings.is_empty() && crate::chrome::chip(ui, false, "Clear all").clicked() {
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
/// The TLE tab: which satellites the tracker follows, and what they are on.
///
/// Three sections, in the order an operator gets to them: subscriptions (the
/// answer for anything they mean to keep tracking, because a TLE goes stale in
/// days), element sets pasted in by hand (the answer for a one-off), and the
/// frequency table the pass window shows.
fn settings_tle_tab(ui: &mut egui::Ui, io: &mut SettingsIo) {
    use crate::theme;

    ui.label(
        RichText::new("Satellites: element sets and frequencies")
            .size(14.0)
            .strong()
            .color(theme::CYAN),
    );
    ui.add_space(4.0);
    if cfg!(target_arch = "wasm32") {
        ui.label(
            RichText::new(
                "The tracker runs in the native app; this tab configures it there. The solar \
                 view in the browser is fed by the server's relay.",
            )
            .weak(),
        );
        return;
    }
    ui.label(
        RichText::new(
            "The tracker already fetches CelesTrak's amateur group on its own. This is for \
             everything else: the NOAA weather birds, a cubesat too new to be in the group, or \
             a fresher element set than the one that arrived.",
        )
        .weak(),
    );
    if !io.sat_ui.note.is_empty() {
        ui.add_space(4.0);
        ui.label(RichText::new(&io.sat_ui.note).color(theme::YELLOW).size(11.0));
    }

    ui.add_space(10.0);
    settings_tle_subscriptions(ui, io);
    ui.add_space(12.0);
    ui.separator();
    ui.add_space(8.0);
    settings_tle_pasted(ui, io);
    ui.add_space(12.0);
    ui.separator();
    ui.add_space(8.0);
    settings_tle_freqs(ui, io);
}

/// Subscribed element-set listings, and the one-click CelesTrak groups.
fn settings_tle_subscriptions(ui: &mut egui::Ui, io: &mut SettingsIo) {
    use crate::theme;

    ui.label(RichText::new("Subscriptions").strong());
    ui.label(
        RichText::new(
            "Listings fetched and kept current, on the same six-hourly cadence as the amateur \
             set. Refreshed while the solar window is open, and by UPDATE NOW here.",
        )
        .weak()
        .size(11.0),
    );
    ui.add_space(6.0);

    let mut remove = None;
    for (i, sub) in io.sat_edit.subs.iter_mut().enumerate() {
        let st = io.sat_subs.iter().find(|s| s.url.trim() == sub.url.trim());
        ui.push_id(("tle-sub", i), |ui| {
            ui.horizontal(|ui| {
                ui.checkbox(&mut sub.enabled, "").on_hover_text("Fetch and track this listing");
                ui.add(
                    egui::TextEdit::singleline(&mut sub.name)
                        .desired_width(120.0)
                        .hint_text("name"),
                );
                ui.add(
                    egui::TextEdit::singleline(&mut sub.url)
                        .desired_width(300.0)
                        .hint_text("https://…"),
                );
                if ui.button("✕").on_hover_text("Remove this subscription").clicked() {
                    remove = Some(i);
                }
            });
            ui.horizontal(|ui| {
                ui.add_space(24.0);
                ui.label(RichText::new("Orbits").color(theme::CYAN_DIM).size(9.5).strong())
                    .on_hover_text(
                        "Which satellites in this listing get an orbit ring and a label. A whole \
                         group wants \"curated\": ninety rings at once is unreadable, and none \
                         at all leaves ninety anonymous dots.",
                    );
                // The middle position keys off sdroxide's own curated list,
                // which is ten *amateur* satellites — so for a weather or GNSS
                // listing it would behave exactly like "none". Greyed out once
                // a fetch has proved this listing has none of them, rather than
                // left as a chip that quietly does nothing.
                let no_curated = st.is_some_and(|s| s.fetched_unix > 0 && s.curated == 0);
                for o in sdroxide_types::OrbitRings::ALL {
                    let dead = o == sdroxide_types::OrbitRings::Curated && no_curated;
                    let resp = ui
                        .add_enabled_ui(!dead, |ui| {
                            crate::chrome::chip(ui, sub.orbits == o, o.label())
                        })
                        .inner;
                    let hint = if dead {
                        "Nothing in this listing is in sdroxide's curated list — that list is                          ten amateur satellites, so this would behave exactly like \"none\"."
                    } else {
                        o.hint()
                    };
                    if resp.on_hover_text(hint).clicked() && !dead {
                        sub.orbits = o;
                    }
                }
                let mut only = sub.only_text();
                let resp = ui
                    .add(
                        egui::TextEdit::singleline(&mut only)
                            .desired_width(180.0)
                            .hint_text("all satellites"),
                    )
                    .on_hover_text(
                        "Catalogue numbers to keep, comma separated. Empty tracks everything the \
                         listing carries.",
                    );
                if resp.changed() {
                    sub.set_only_text(&only);
                }

                // Status: what the last fetch actually did. Matched by URL
                // rather than by position — the two lists are edited apart.
                let (text, color) = match (sub.problem(), st) {
                    (Some(p), _) => (p.to_string(), theme::PINK),
                    (None, None) => ("not fetched yet".to_string(), theme::LINE_LIT),
                    (None, Some(s)) => match &s.error {
                        Some(e) => (e.clone(), theme::PINK),
                        None if s.fetched_unix == 0 => {
                            ("not fetched yet".to_string(), theme::LINE_LIT)
                        }
                        None => (
                            format!(
                                "{} satellites · {} old",
                                s.count,
                                sdroxide_solar::timefmt::age(now_unix() - s.fetched_unix)
                            ),
                            theme::GREEN,
                        ),
                    },
                };
                ui.label(RichText::new(text).color(color).size(10.5));
            });
        });
        ui.add_space(2.0);
    }
    if let Some(i) = remove {
        io.sat_edit.subs.remove(i);
    }

    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        if ui.button("+ Subscription").clicked() {
            io.sat_edit.subs.push(sdroxide_types::TleSubscription::new("New", ""));
        }
        if crate::chrome::chip_accent(
            ui,
            false,
            RichText::new(" UPDATE NOW ").strong(),
            theme::GREEN,
            theme::INK_ON_CYAN,
        )
        .on_hover_text("Fetch every enabled subscription now")
        .clicked()
        {
            *io.sat_sub_refresh = true;
        }
    });

    ui.add_space(6.0);
    ui.label(RichText::new("CelesTrak groups").color(theme::CYAN_DIM).size(10.0).strong());
    ui.horizontal_wrapped(|ui| {
        for g in sdroxide_types::CELESTRAK_GROUPS {
            let have = io.sat_edit.has_sub(g.url);
            if crate::chrome::chip(ui, have, g.name).on_hover_text(g.hint).clicked() && !have {
                let mut sub = sdroxide_types::TleSubscription::new(g.name, g.url);
                sub.orbits = g.orbits;
                io.sat_edit.subs.push(sub);
                io.sat_ui.note = format!("Subscribed to {}. Press UPDATE NOW to fetch it.", g.name);
            }
        }
    });
}

/// Element sets pasted in by hand.
fn settings_tle_pasted(ui: &mut egui::Ui, io: &mut SettingsIo) {
    use crate::theme;

    ui.label(RichText::new("Pasted element sets").strong());
    ui.label(
        RichText::new(
            "For a one-off. These do not update themselves, and SGP4 stops propagating an \
             element set once it is a fortnight past its epoch — subscribe instead for anything \
             you mean to keep.",
        )
        .weak()
        .size(11.0),
    );
    ui.add_space(6.0);

    let now = now_unix();
    let mut remove = None;
    for (i, t) in io.sat_edit.tles.iter_mut().enumerate() {
        ui.push_id(("tle-set", i), |ui| {
            ui.horizontal(|ui| {
                ui.checkbox(&mut t.enabled, "").on_hover_text("Track this one");
                ui.add(
                    egui::TextEdit::singleline(&mut t.name).desired_width(180.0).hint_text("name"),
                );
                match t.problem() {
                    Some(p) => {
                        ui.label(RichText::new(p).color(theme::PINK).size(10.5));
                    }
                    None => {
                        let age = tle_epoch_age(t, now);
                        let (text, color) = match age {
                            // Past where SGP4 is worth anything, which is the
                            // whole reason a paste is a stopgap.
                            Some(a) if a > 14 * 86_400 => (
                                format!(
                                    "NORAD {} · {} old — too stale to propagate",
                                    t.norad_id().unwrap_or(0),
                                    sdroxide_solar::timefmt::age(a)
                                ),
                                theme::PINK,
                            ),
                            Some(a) if a > 3 * 86_400 => (
                                format!(
                                    "NORAD {} · {} old",
                                    t.norad_id().unwrap_or(0),
                                    sdroxide_solar::timefmt::age(a)
                                ),
                                theme::YELLOW,
                            ),
                            Some(a) => (
                                format!(
                                    "NORAD {} · {} old",
                                    t.norad_id().unwrap_or(0),
                                    sdroxide_solar::timefmt::age(a)
                                ),
                                theme::GREEN,
                            ),
                            None => {
                                (format!("NORAD {}", t.norad_id().unwrap_or(0)), theme::LINE_LIT)
                            }
                        };
                        ui.label(RichText::new(text).color(color).size(10.5));
                    }
                }
                if ui.button("✎").on_hover_text("Show the two element lines").clicked() {
                    io.sat_ui.open_tle = (io.sat_ui.open_tle != Some(i)).then_some(i);
                }
                if ui.button("✕").on_hover_text("Remove").clicked() {
                    remove = Some(i);
                }
            });
            if io.sat_ui.open_tle == Some(i) {
                // Monospace: the format is column-addressed, so a proportional
                // font makes a misaligned paste impossible to see.
                for line in [&mut t.line1, &mut t.line2] {
                    ui.add(
                        egui::TextEdit::singleline(line)
                            .desired_width(560.0)
                            .font(egui::TextStyle::Monospace),
                    );
                }
            }
        });
    }
    if let Some(i) = remove {
        io.sat_edit.tles.remove(i);
        io.sat_ui.open_tle = None;
    }

    ui.add_space(6.0);
    ui.add(
        egui::TextEdit::multiline(&mut io.sat_ui.paste)
            .desired_rows(3)
            .desired_width(600.0)
            .font(egui::TextStyle::Monospace)
            .hint_text("Paste two- or three-line element sets here"),
    );
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        if ui.button("+ Add pasted").clicked() {
            let found = sdroxide_types::parse_tle_block(&io.sat_ui.paste);
            io.sat_ui.note =
                match found.len() {
                    0 => "Nothing in the paste box looked like an element set.".to_string(),
                    n => {
                        // Replace rather than duplicate: pasting a fresher set for
                        // a satellite already listed is the common case, and a
                        // second entry for the same catalogue number would leave
                        // whichever came first winning at random.
                        let mut replaced = 0;
                        for t in found {
                            match io.sat_edit.tles.iter().position(|e| {
                                e.norad_id().is_some() && e.norad_id() == t.norad_id()
                            }) {
                                Some(k) => {
                                    // Keep the operator's own name and their
                                    // enabled/disabled choice; take the elements.
                                    io.sat_edit.tles[k].line1 = t.line1;
                                    io.sat_edit.tles[k].line2 = t.line2;
                                    replaced += 1;
                                }
                                None => io.sat_edit.tles.push(t),
                            }
                        }
                        io.sat_ui.paste.clear();
                        match replaced {
                            0 => format!("Added {n} element set(s)."),
                            r => format!("Added {} and refreshed {r} element set(s).", n - r),
                        }
                    }
                };
        }
        if ui.button("Clear box").clicked() {
            io.sat_ui.paste.clear();
        }
    });
}

/// Age of a pasted element set, in seconds, from the epoch in columns 19–32 of
/// line 1.
///
/// Its own parse rather than SGP4's, because this has to work on an entry the
/// propagator would reject — the whole point is to say *why* it is being
/// rejected.
fn tle_epoch_age(t: &sdroxide_types::CustomTle, now_unix: i64) -> Option<i64> {
    let l1 = t.line1.as_bytes();
    if l1.len() < 32 {
        return None;
    }
    let field = std::str::from_utf8(&l1[18..32]).ok()?.trim();
    let yy: i64 = field.get(..2)?.parse().ok()?;
    let doy: f64 = field.get(2..)?.parse().ok()?;
    // Two-digit years: 57–99 are 1957 onwards, 00–56 are 2000 onwards. That is
    // the convention the format itself carries.
    let year = if yy < 57 { 2000 + yy } else { 1900 + yy };
    let jan1 = sdroxide_types::ymd_hms_to_unix(year, 1, 1, 0, 0, 0);
    Some(now_unix - (jan1 + ((doy - 1.0) * 86_400.0) as i64))
}

/// The frequency table the pass window shows: the operator's entries, which
/// override the built-in one satellite for satellite.
fn settings_tle_freqs(ui: &mut egui::Ui, io: &mut SettingsIo) {
    use crate::theme;

    ui.label(RichText::new("Frequencies").strong());
    ui.label(
        RichText::new(
            "Shown under the pass table in the solar window. An entry here replaces the \
             built-in one for that catalogue number outright, so start from a copy of it unless \
             you mean to drop the rest.",
        )
        .weak()
        .size(11.0),
    );
    ui.add_space(6.0);

    let mut remove = None;
    for (i, f) in io.sat_edit.freqs.iter_mut().enumerate() {
        ui.push_id(("sat-freq", i), |ui| {
            ui.horizontal(|ui| {
                let open = io.sat_ui.open_freq == Some(i);
                if ui.button(if open { "▼" } else { "▶" }).clicked() {
                    io.sat_ui.open_freq = (!open).then_some(i);
                }
                ui.label(RichText::new(format!("NORAD {}", f.norad_id)).color(theme::CYAN_DIM));
                ui.add(
                    egui::TextEdit::singleline(&mut f.name).desired_width(180.0).hint_text("name"),
                );
                ui.label(
                    RichText::new(format!("{} link(s)", f.links.len()))
                        .color(theme::LINE_LIT)
                        .size(10.5),
                );
                if ui.button("✕").on_hover_text("Remove this satellite's entry").clicked() {
                    remove = Some(i);
                }
            });
            if io.sat_ui.open_freq != Some(i) {
                return;
            }
            let mut drop_link = None;
            egui::Grid::new("sat-links").num_columns(6).spacing([8.0, 4.0]).show(ui, |ui| {
                for h in ["LINK", "DOWNLINK", "UPLINK", "MODE", "NOTE", ""] {
                    ui.label(RichText::new(h).color(theme::CYAN_DIM).size(9.5).strong());
                }
                ui.end_row();
                for (k, l) in f.links.iter_mut().enumerate() {
                    ui.add(
                        egui::TextEdit::singleline(&mut l.label)
                            .desired_width(120.0)
                            .hint_text("FM repeater"),
                    );
                    freq_box(ui, (k, "down"), &mut l.downlink, "145.800");
                    freq_box(ui, (k, "up"), &mut l.uplink, "435.250");
                    ui.add(
                        egui::TextEdit::singleline(&mut l.mode).desired_width(90.0).hint_text("FM"),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut l.note)
                            .desired_width(180.0)
                            .hint_text("CTCSS 67.0 Hz"),
                    );
                    if ui.button("✕").clicked() {
                        drop_link = Some(k);
                    }
                    ui.end_row();
                }
            });
            if let Some(k) = drop_link {
                f.links.remove(k);
            }
            ui.horizontal(|ui| {
                if ui.button("+ Link").clicked() {
                    f.links.push(Default::default());
                }
                // The built-in row is almost always what you want to start
                // from: correcting one frequency should not mean retyping the
                // beacon, the telemetry and the transponder as well.
                if let Some(b) = sdroxide_solar::satfreq::builtin_for(f.norad_id) {
                    if ui
                        .button("Copy built-in")
                        .on_hover_text(format!(
                            "Replace these links with the built-in ones for {}",
                            b.name
                        ))
                        .clicked()
                    {
                        f.links = b.links.clone();
                        if f.name.trim().is_empty() {
                            f.name = b.name.clone();
                        }
                    }
                }
            });
            ui.add_space(4.0);
            ui.label(
                RichText::new(
                    "A frequency is either one number (145.800) or a transponder passband \
                     written 145.950-145.970. Leave a direction blank for a beacon.",
                )
                .weak()
                .size(10.0),
            );
        });
        ui.add_space(2.0);
    }
    if let Some(i) = remove {
        io.sat_edit.freqs.remove(i);
        io.sat_ui.open_freq = None;
    }

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut io.sat_ui.new_freq_id)
                .desired_width(80.0)
                .hint_text("NORAD"),
        );
        ui.add(
            egui::TextEdit::singleline(&mut io.sat_ui.new_freq_name)
                .desired_width(160.0)
                .hint_text("name"),
        );
        if ui.button("+ Satellite").clicked() {
            match io.sat_ui.new_freq_id.trim().parse::<u64>() {
                Ok(id) if id > 0 => {
                    let name = io.sat_ui.new_freq_name.trim().to_string();
                    let existed = io.sat_edit.freqs_for(id).is_some();
                    let entry = io.sat_edit.freqs_for_mut(id, &name);
                    // Seed from the built-in table when there is one: an entry
                    // that starts empty shadows it, which reads as the
                    // frequencies having been deleted.
                    if !existed {
                        if let Some(b) = sdroxide_solar::satfreq::builtin_for(id) {
                            entry.links = b.links.clone();
                            if entry.name.trim().is_empty() {
                                entry.name = b.name.clone();
                            }
                        } else {
                            entry.links.push(Default::default());
                        }
                    }
                    io.sat_ui.open_freq = io.sat_edit.freqs.iter().position(|f| f.norad_id == id);
                    io.sat_ui.new_freq_id.clear();
                    io.sat_ui.new_freq_name.clear();
                    io.sat_ui.note.clear();
                }
                _ => {
                    io.sat_ui.note =
                        "A frequency entry needs the satellite's NORAD catalogue number."
                            .to_string()
                }
            }
        }
    });
}

/// A frequency box that edits an optional passband in place.
///
/// Kept as text only while it is being typed into: parsing on every keystroke
/// would fight a half-typed "145." by turning it into 145.000 under the cursor.
fn freq_box(
    ui: &mut egui::Ui,
    salt: impl std::hash::Hash + std::fmt::Debug,
    band: &mut Option<sdroxide_types::Passband>,
    hint: &str,
) {
    let id = ui.id().with(("freqbox", salt));
    let mut text = ui
        .data_mut(|d| d.get_temp::<String>(id))
        .unwrap_or_else(|| band.map(|b| b.to_string()).unwrap_or_default());
    let resp = ui.add(egui::TextEdit::singleline(&mut text).desired_width(110.0).hint_text(hint));
    if resp.changed() {
        *band = sdroxide_types::Passband::parse(&text);
        ui.data_mut(|d| d.insert_temp(id, text));
    } else if resp.lost_focus() {
        // Drop the in-progress text so the box re-derives from what was
        // actually stored — a half-typed "145." must not keep showing as if it
        // were a frequency the table holds.
        ui.data_mut(|d| d.remove_temp::<String>(id));
    }
}

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

    ui.label(RichText::new("Hamlib rigctld server").size(14.0).strong().color(crate::theme::CYAN));
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
                ui.label(
                    RichText::new(format!("Failed: {e}")).color(Color32::from_rgb(230, 90, 80)),
                );
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
            RichText::new(
                "The TCI server runs alongside the radio engine, so it can only be \
                           configured from the native app.",
            )
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
                    self.session_qsos += 1;
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
                RadioEvent::HellColumns { seq, rows, cols } => {
                    self.hell.on_columns(seq, rows, &cols, &self.view.hell, &ctx);
                }
                RadioEvent::WefaxLine { image_id, y, gray } => {
                    self.wefax.push_line(image_id, y, &gray);
                }
                RadioEvent::WefaxImage { png, .. } => {
                    // The engine has already written the file; the gallery entry
                    // is named by the same rule against the same clock and dial,
                    // so it carries the date and station the file on disk does.
                    // A remote client, which has no file, gets the label anyway.
                    let dial = self.state.rx_freq_hz();
                    let name = sdroxide_types::WefaxChartMeta {
                        unix: crate::time::now_unix(),
                        dial_hz: (dial > 0.0).then_some(dial),
                    }
                    .file_name();
                    self.wefax.add_chart(&ctx, &name, &png);
                    self.wefax.clear_live();
                }
                RadioEvent::WefaxStatus(s) => self.wefax.status = s,
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
                RadioEvent::RifpRows { image_id, y, w, h, rows } => {
                    self.sstv.on_rifp_rows(image_id, y, w, h, &rows, &ctx);
                }
                RadioEvent::RifpImage { image_id, meta, png } => {
                    self.sstv.on_rifp_image(image_id, meta, &png, &ctx);
                }
                RadioEvent::RifpStatus(s) => {
                    self.sstv.rifp = s;
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
        // An open manual takes the scrolling keys before the bindings run, so
        // reading it never tunes the radio at the same time.
        self.help.grab_keys(&ctx);
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
                            RichText::new("⚠").size(15.0).color(Color32::from_rgb(255, 190, 70)),
                        );
                        ui.label(
                            RichText::new(notice)
                                .size(13.0)
                                .color(Color32::from_rgb(240, 220, 180)),
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
        // The broadcast stations are refreshed first, before anything reads
        // them: the overlay here, the SPOTS list and the world map all do.
        self.refresh_broadcast_spots(now_unix());
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
            } else if mode == Mode::Js8 {
                // Worth showing: Turbo's 160 Hz footprint against Slow's 25 Hz
                // is what decides whether a frequency is actually free.
                let bw = self
                    .digi_status
                    .as_ref()
                    .and_then(|s| s.js8.as_ref())
                    .map_or(50.0, |j| j.speed.bandwidth_hz());
                vec![audio_hz, audio_hz + bw]
            } else if mode == Mode::Fsq {
                let baud = self.digi_status.as_ref().map(|s| s.config.fsq_baud).unwrap_or(4.5);
                let bw = 33.0 * baud;
                vec![audio_hz - bw / 2.0, audio_hz + bw / 2.0]
            } else if mode == Mode::Hell {
                let v =
                    self.digi_status.as_ref().map(|s| s.config.hell_variant).unwrap_or_default();
                let bw = v.bandwidth_hz() as f32;
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
                    if mode == Mode::Ft8 {
                        self.digi_status.as_ref().map(|s| s.config.dxped_mode).unwrap_or_default()
                    } else {
                        sdroxide_types::DxpedMode::Normal
                    },
                    mode.is_slotted()
                        && self.digi_status.as_ref().map(|s| s.config.auto_tx_freq).unwrap_or(true),
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
            let hresp = crate::chrome::split_handle(
                ui,
                egui::vec2(width, handle_h),
                Some(crate::theme::PANEL),
            );
            if hresp.dragged() {
                // Drag down shrinks the panel (waterfall grows), drag up grows it.
                let d = hresp.drag_delta().y / total;
                self.view.digi_panel_fraction =
                    (self.view.digi_panel_fraction - d).clamp(0.2, 0.82);
            }
            ui.allocate_ui(egui::vec2(width, panel_h), |ui| {
                egui::Frame::new()
                    .fill(crate::theme::BG_DEEP)
                    .inner_margin(egui::Margin { left: 0, right: 0, top: 6, bottom: 0 })
                    .show(ui, |ui| {
                        crate::chrome::angled_frame(ui, crate::theme::PINK, |ui| {
                            if mode.is_rade() {
                                self.rade_panel(ui, &mut cmds, panel_h);
                            } else if mode.is_wefax() {
                                self.wefax_panel(ui, &mut cmds, panel_h);
                            } else if mode.is_image() {
                                self.image_panel(ui, &mut cmds, mode);
                            } else if mode.is_rf_paint() {
                                self.rf_paint_panel(ui, &mut cmds, panel_h);
                            } else if mode.is_fsq() {
                                self.fsq_panel(ui, &mut cmds, panel_h);
                            } else if mode.is_hell() {
                                self.hell_panel(ui, &mut cmds, panel_h);
                            } else if is_text {
                                self.text_modem_panel(ui, &mut cmds, panel_h);
                            } else if mode.is_js8() {
                                self.js8_panel(ui, &mut cmds, panel_h);
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
        // Last, so it lands on top of everything else that opened this frame.
        self.oob_tx_window(&ctx);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let grid = self.my_grid();
            let traffic = self.digi_traffic(ctx.input(|i| i.time));
            // Only while the window is open: walking the whole logbook is not
            // free, and the closed window has nothing to paint it on.
            let awards = if self.solar.open { self.award_heat() } else { Default::default() };
            self.solar.viewport(&ctx, &grid, traffic, awards, std::sync::Arc::clone(&self.sat_cfg));
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

#[cfg(not(target_arch = "wasm32"))]
fn load_sat_config() -> sdroxide_types::SatConfig {
    sdroxide_config::load_sat_config()
}
/// The browser tab has no satellite tracker of its own — the solar view there
/// is fed by the server's relay — so there is nothing to configure and nothing
/// to load.
#[cfg(target_arch = "wasm32")]
fn load_sat_config() -> sdroxide_types::SatConfig {
    Default::default()
}

#[cfg(not(target_arch = "wasm32"))]
fn persist_sat_config(cfg: &sdroxide_types::SatConfig) {
    if let Err(e) = sdroxide_config::save_sat_config(cfg) {
        eprintln!("failed to save the satellite config: {e}");
    }
}

// ── Broadcast stations (native: seeded config-dir JSON; wasm: the bundled table)
#[cfg(not(target_arch = "wasm32"))]
fn load_broadcast_stations() -> Vec<sdroxide_types::BroadcastStation> {
    sdroxide_config::load_broadcast_stations()
}
#[cfg(not(target_arch = "wasm32"))]
fn restore_bundled_broadcast_stations() {
    if let Err(e) = sdroxide_config::restore_bundled_broadcast_stations() {
        eprintln!("failed to restore the bundled broadcast station list: {e}");
    }
}

/// The browser tab has no config directory to seed, so it gets the table
/// compiled into the wasm bundle — the same data, just not editable there.
#[cfg(target_arch = "wasm32")]
fn load_broadcast_stations() -> Vec<sdroxide_types::BroadcastStation> {
    sdroxide_types::broadcast::builtin().to_vec()
}
#[cfg(target_arch = "wasm32")]
fn restore_bundled_broadcast_stations() {}
#[cfg(target_arch = "wasm32")]
fn persist_sat_config(_cfg: &sdroxide_types::SatConfig) {}

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

/// The broadcast-station block on the Spots settings tab: where the list lives,
/// and the two things that can be done to it from here.
#[cfg(not(target_arch = "wasm32"))]
fn broadcast_stations_settings(ui: &mut egui::Ui, reload: &mut bool, restore: &mut bool) {
    let path = sdroxide_config::broadcast_stations_path();
    ui.label(
        RichText::new(
            "The longwave and shortwave stations labelled on the waterfall. Seeded from the \
             bundled list on first run, then yours to edit — sdroxide never overwrites it.",
        )
        .weak(),
    );
    if let Ok(p) = &path {
        ui.horizontal(|ui| {
            ui.add(
                egui::Label::new(
                    RichText::new(p.display().to_string()).monospace().size(10.5).color(
                        Color32::from_gray(150),
                    ),
                )
                .truncate(),
            );
        });
    }
    ui.horizontal(|ui| {
        if ui
            .button("Reload")
            .on_hover_text("Re-read the file after editing it")
            .clicked()
        {
            *reload = true;
        }
        if ui
            .button("Restore bundled list")
            .on_hover_text("Replace the file with the one shipped in this build (the old one is kept as .json.bak)")
            .clicked()
        {
            *restore = true;
        }
    });
}

/// The browser client reads the table compiled into the wasm bundle, so there is
/// no file to point at and nothing to reload.
#[cfg(target_arch = "wasm32")]
fn broadcast_stations_settings(ui: &mut egui::Ui, _reload: &mut bool, _restore: &mut bool) {
    ui.label(
        RichText::new(
            "The broadcast stations labelled on the waterfall come from the list built into \
             this build. Editing them needs the desktop app.",
        )
        .weak(),
    );
}

/// Everything about a spot the search box should be able to find it by.
///
/// The frequency goes in twice, as kHz and as MHz, because a shortwave listener
/// thinks in `9420` and a ham in `9.420` and both should work. The kind label is
/// in there too, so typing `bc` narrows to the broadcast stations without having
/// to reach for the chips.
fn spot_haystack(s: &Spot) -> String {
    let mut h = String::with_capacity(96);
    h.push_str(&s.call);
    for extra in [
        s.kind.label(),
        &s.mode,
        &s.comment,
        s.reference.as_deref().unwrap_or(""),
        &s.spotter,
        s.grid.as_deref().unwrap_or(""),
    ] {
        if !extra.is_empty() {
            h.push(' ');
            h.push_str(extra);
        }
    }
    h.push_str(&format!(" {:.0} {:.4}", s.freq_hz / 1e3, s.freq_hz / 1e6));
    h
}

/// One clickable spot row for the spots window: kind badge, call, frequency,
/// mode, age or schedule, and the park/summit/transmitter reference or comment.
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
                col(
                    ui,
                    44.0,
                    egui::Label::new(
                        RichText::new(s.kind.label()).size(10.0).strong().color(kind_col),
                    ),
                );
                col(
                    ui,
                    132.0,
                    egui::Label::new(
                        RichText::new(&s.call).size(14.0).strong().color(crate::theme::TEXT_STRONG),
                    )
                    .truncate(),
                );
                col(
                    ui,
                    78.0,
                    egui::Label::new(
                        RichText::new(format!("{:.4}", s.freq_hz / 1e6))
                            .monospace()
                            .size(12.0)
                            .color(gray),
                    ),
                );
                col(
                    ui,
                    46.0,
                    egui::Label::new(RichText::new(&s.mode).monospace().size(11.0).color(gray)),
                );
                // A broadcast station is not a report that ages: it carries its
                // schedule (`"24h"`, `"1800-2100"`) in this column instead.
                let when = if s.kind == SpotKind::Broadcast {
                    s.spotter.clone()
                } else {
                    fmt_age(now_utc - s.when_utc)
                };
                col(
                    ui,
                    76.0,
                    egui::Label::new(RichText::new(when).size(10.5).color(Color32::from_gray(120))),
                );
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
        // JS8 conventional dials; signals sit in the ~3 kHz above each.
        Mode::Js8 => &[
            ("160m", 1_842_000.0),
            ("80m", 3_578_000.0),
            ("40m", 7_078_000.0),
            ("30m", 10_130_000.0),
            ("20m", 14_078_000.0),
            ("17m", 18_104_000.0),
            ("15m", 21_078_000.0),
            ("12m", 24_922_000.0),
            ("10m", 28_078_000.0),
        ],
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
        // Hellschreiber (USB dial), from hellschreiber.com's narrow-band digimode
        // band plan of 18 March 2019 — its "common calling & operating" column,
        // taking IARU Region 1 where that column is split, to match the Region 1
        // defaults `Band::edges` already uses.
        //
        // Two deliberate departures, on 15 m and 10 m: that table's own calling
        // frequencies there (21074 / 28074) fall *outside* the operating ranges
        // it lists in the same cell, and both sit exactly on the FT8 sub-band.
        // The range starts are used instead — internally consistent, clear of
        // FT8, and what the Feld Hell Club lists. 6 m is not in that table at
        // all, so it keeps the club's figure.
        Mode::Hell => &[
            ("160m", 1_840_000.0),
            ("80m", 3_574_000.0),
            ("60m", 5_351_500.0),
            ("40m", 7_040_000.0),
            ("30m", 10_144_000.0),
            ("20m", 14_073_000.0),
            ("17m", 18_104_000.0),
            ("15m", 21_063_000.0),
            ("12m", 24_924_000.0),
            ("10m", 28_063_000.0),
            ("6m", 50_286_000.0),
        ],
        // RF Paint has no defined calling frequency — offer no band presets.
        Mode::RfPaint => &[],
        // RIFP assigns no frequency at all: 433.92 MHz is the deployment
        // example the draft names, and the others are the middle of the
        // segments where a 25 kHz channel is a realistic thing to ask for
        // (10 m FM and the 6 m all-modes part). The dial is the signal's
        // *centre* here, not its lower edge as in every mode above.
        Mode::Rifp => &[
            ("10m", 29_600_000.0),
            ("6m", 51_250_000.0),
            // The 2 m image/facsimile corner: 144.700 is the FAX calling
            // frequency, inside the all-modes segment.
            ("2m", 144_700_000.0),
            ("70cm", sdroxide_types::RIFP_CALLING_HZ),
        ],
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

/// The hover card behind a decode row: everything the entity file, the log and
/// the operator's own grid already know about this station, said in full.
///
/// The row can only afford a callsign, a grid and two numbers; all of this is
/// resolved for it anyway, so the card costs nothing but the space to show it.
#[allow(clippy::too_many_arguments)]
fn station_card(
    ui: &mut egui::Ui,
    d: &Decode,
    entity: Option<sdroxide_types::EntityInfo>,
    dist_km: Option<f64>,
    my_grid: &str,
    novelty: sdroxide_types::Novelty,
    band: &str,
    queued: bool,
    cq_for_us: bool,
) {
    ui.set_max_width(300.0);
    let dim = Color32::from_gray(140);
    match d.from.as_deref() {
        Some(call) => {
            ui.label(RichText::new(call).size(16.0).strong().color(crate::theme::TEXT_STRONG));
        }
        None if d.free_text => {
            ui.label(RichText::new("free text").size(13.0).italics().color(dim));
        }
        // A hashed callsign nobody on this receiver has heard in full yet.
        None => {
            ui.label(RichText::new("hashed callsign, not yet heard in full").size(13.0).color(dim));
        }
    }

    match entity {
        Some(e) => {
            ui.label(
                RichText::new(e.name)
                    .size(13.0)
                    .strong()
                    .color(crate::theme::continent_color(e.continent)),
            );
            ui.label(
                RichText::new(format!(
                    "{} · CQ zone {} · ITU zone {}",
                    e.continent, e.cq_zone, e.itu_zone
                ))
                .size(11.5)
                .color(dim),
            );
        }
        None if d.from.is_some() => {
            ui.label(RichText::new("entity unknown").size(11.5).color(dim));
        }
        None => {}
    }

    // Where they are, from their grid: distance and the beam heading to point.
    if let Some(g) = d.grid.as_deref() {
        let bearing =
            (!my_grid.is_empty()).then(|| sdroxide_types::grid_bearing(my_grid, g)).flatten();
        let mut line = g.to_string();
        if let Some(km) = dist_km {
            line.push_str(&format!(" · {km:.0} km"));
        }
        if let Some(b) = bearing {
            line.push_str(&format!(" · {b:.0}°"));
        }
        ui.label(RichText::new(line).size(12.0).color(crate::theme::YELLOW));
    }

    ui.separator();
    // Worked before? The one thing that decides whether this decode is worth
    // acting on, spelled out rather than compressed into a four-letter badge.
    let band_label = if band.is_empty() { "this band".to_string() } else { band.to_string() };
    let (worked, col) = if novelty.new_dxcc {
        ("New entity — never worked, on any band".to_string(), crate::theme::PINK)
    } else if novelty.new_dxcc_band {
        (format!("New entity on {band_label}"), crate::theme::YELLOW)
    } else if novelty.new_grid {
        ("New grid square".to_string(), crate::theme::CYAN)
    } else if novelty.new_call {
        ("Not worked before".to_string(), crate::theme::CYAN_DIM)
    } else if novelty.dupe {
        (format!("Worked before on {band_label}"), Color32::from_gray(130))
    } else {
        ("Worked before, but not on this band".to_string(), Color32::from_gray(150))
    };
    ui.label(RichText::new(worked).size(12.0).color(col));

    if let Some(target) = d.cq_to.as_deref() {
        ui.label(
            RichText::new(if cq_for_us {
                format!("Calling CQ {target} — that includes you")
            } else {
                format!("Calling CQ {target} — not aimed at you")
            })
            .size(11.5)
            .color(if cq_for_us { crate::theme::GREEN } else { dim }),
        );
    }
    if queued {
        ui.label(RichText::new("In the call queue").size(11.5).color(crate::theme::GREEN));
    }
    ui.label(
        RichText::new(format!("{:+} dB · {:.0} Hz · DT {:+.1} s", d.snr_db, d.audio_hz, d.dt))
            .size(11.0)
            .monospace()
            .color(dim),
    );
}

/// One fixed-width column of a station row, shared by the FT8 decode list and
/// the JS8 heard list so the two line up field for field.
///
/// The width is *reserved*, not requested: a plain `allocate_ui` shrinks to its
/// content, so a short callsign would collapse the column and shift everything
/// after it out of alignment down the list.
fn row_cell(ui: &mut egui::Ui, w: f32, h: f32, align_right: bool, lbl: egui::Label) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::hover());
    let layout = if align_right {
        egui::Layout::right_to_left(egui::Align::Center)
    } else {
        egui::Layout::left_to_right(egui::Align::Center)
    };
    ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(layout)).add(lbl);
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
    /// Where a RIFP picture came from and how it was carried, for the caption
    /// under the enlarged view. `None` for SSTV, which carries no metadata.
    rifp: Option<RifpMeta>,
    tex: egui::TextureHandle,
}

/// Image-panel state, shared by SSTV and RIFP: received gallery, in-progress
/// incoming picture, transmit slots, the overlay message, the current mode, and
/// cached textures.
///
/// One workspace for both modes on purpose. The pictures an operator wants to
/// send, the captions on them, and the pictures that came back are the same
/// things whichever protocol carried them; only the control strip and the
/// transmit sizing differ.
struct SstvUi {
    tx_mode: SstvMode,
    /// Latest RIFP engine status (transfer progress, sessions, counters).
    rifp: RifpStatus,
    /// Size of the picture currently arriving, so the live canvas can be built
    /// before the whole object is in. `(0, 0)` when nothing is arriving.
    rx_dims: (u16, u16),
    /// Size the cached preview was composed at, so a change of transmit size
    /// rebuilds it.
    preview_dims: (u16, u16),
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
            rifp: RifpStatus::default(),
            rx_dims: (0, 0),
            preview_dims: (0, 0),
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
            self.rx_color =
                Some(crate::sstv::color_image(&vec![0u8; w as usize * h as usize * 3], w, h));
        }
        let Some(ci) = self.rx_color.as_mut() else { return };
        let (w, h) = (w as usize, h as usize);
        if (y as usize) < h && rgb.len() >= w * 3 {
            let row = y as usize * w;
            for x in 0..w {
                ci.pixels[row + x] = Color32::from_rgb(rgb[x * 3], rgb[x * 3 + 1], rgb[x * 3 + 2]);
            }
        }
        self.rx_tex = Some(ctx.load_texture("sstv_rx", ci.clone(), egui::TextureOptions::NEAREST));
    }

    /// A completed image arrived: decode and add it to the gallery.
    fn on_image(
        &mut self,
        _id: u32,
        mode: SstvMode,
        _w: u16,
        _h: u16,
        png: &[u8],
        ctx: &egui::Context,
    ) {
        if let Some((rgb, w, h)) = crate::sstv::decode_image(png) {
            let ci = crate::sstv::color_image(&rgb, w, h);
            let tex = ctx.load_texture("sstv_recv", ci, egui::TextureOptions::NEAREST);
            self.received.insert(0, SstvRecv { mode: Some(mode), rifp: None, tex });
            self.received.truncate(60);
        }
        self.rx_color = None;
        self.rx_tex = None;
    }

    /// RIFP: reassembled raster rows arrived — paint them into the live
    /// picture. Only the unencoded raster gets here; everything else appears
    /// whole in [`SstvUi::on_rifp_image`].
    fn on_rifp_rows(&mut self, id: u32, y: u16, w: u16, h: u16, gray: &[u8], ctx: &egui::Context) {
        if self.rx_id != id || self.rx_color.is_none() || self.rx_dims != (w, h) {
            self.rx_id = id;
            self.rx_dims = (w, h);
            self.rx_color =
                Some(crate::sstv::color_image(&vec![0u8; w as usize * h as usize * 3], w, h));
        }
        let Some(ci) = self.rx_color.as_mut() else { return };
        let (wu, hu) = (w as usize, h as usize);
        for (row, pixels) in gray.chunks_exact(wu).enumerate() {
            let y = y as usize + row;
            if y >= hu {
                break;
            }
            for (x, &g) in pixels.iter().enumerate() {
                ci.pixels[y * wu + x] = Color32::from_gray(g);
            }
        }
        self.rx_tex = Some(ctx.load_texture("rifp_rx", ci.clone(), egui::TextureOptions::NEAREST));
    }

    /// RIFP: a complete, digest-verified picture arrived.
    fn on_rifp_image(&mut self, _id: u32, meta: RifpMeta, png: &[u8], ctx: &egui::Context) {
        if let Some((rgb, w, h)) = crate::sstv::decode_image(png) {
            let ci = crate::sstv::color_image(&rgb, w, h);
            let tex = ctx.load_texture("rifp_recv", ci, egui::TextureOptions::NEAREST);
            self.received.insert(0, SstvRecv { mode: None, rifp: Some(meta), tex });
            self.received.truncate(60);
        }
        self.rx_color = None;
        self.rx_tex = None;
        self.rx_dims = (0, 0);
    }

    /// The overlay message for the slot currently being edited.
    fn current_message(&self) -> &str {
        self.slot_messages.get(self.selected_slot).map(String::as_str).unwrap_or("")
    }

    /// Persist the per-slot overlay messages to the config file (native only).
    fn save_messages(&self) {
        sstv_save_messages(&self.slot_messages);
    }

    /// Rebuild the transmit preview when the size, slot, or message changed.
    /// `dims` is the transmitted picture size — the SSTV line format's, or the
    /// operator's chosen RIFP size.
    fn ensure_preview(&mut self, dims: (u16, u16), ctx: &egui::Context) {
        if !self.preview_dirty {
            return;
        }
        self.preview_dirty = false;
        let message = self.current_message().to_string();
        match self.slots.get(self.selected_slot).and_then(|s| s.as_ref()) {
            Some(slot) => {
                let (rgb, w, h) = crate::sstv::compose(
                    dims.0,
                    dims.1,
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
    fn compose_png(&self, dims: (u16, u16)) -> Option<Vec<u8>> {
        let slot = self.slots.get(self.selected_slot).and_then(|s| s.as_ref())?;
        let (rgb, w, h) = crate::sstv::compose(
            dims.0,
            dims.1,
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
    /// The image panel, shared by SSTV and RIFP: a live picture and a gallery
    /// on the left, a transmit compositor on the right, and a control strip
    /// that is the only part either mode owns alone.
    fn image_panel(&mut self, ui: &mut egui::Ui, cmds: &mut Vec<Command>, mode: Mode) {
        let ctx = ui.ctx().clone();
        let rifp = mode.is_rifp();
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
        // The transmitted size: SSTV's line format fixes it, RIFP leaves it to
        // the operator. Changing it invalidates the composed preview.
        let dims = if rifp {
            self.digi_cfg_edit.rifp_size.dimensions()
        } else {
            self.sstv.tx_mode.dimensions()
        };
        if self.sstv.preview_dims != dims {
            self.sstv.preview_dims = dims;
            self.sstv.preview_dirty = true;
        }
        self.sstv.ensure_preview(dims, &ctx);
        ctx.request_repaint_after(Duration::from_millis(120));

        let st = self.sstv.status;
        let (signal, tx_active, progress) = if rifp {
            (self.sstv.rifp.signal, self.sstv.rifp.tx_active, self.sstv.rifp.tx_progress)
        } else {
            (st.signal, st.tx_active, st.progress)
        };

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
                            if rifp {
                                self.rifp_controls(ui, cmds);
                                return;
                            }

                            // Mode selection: Auto + the per-mode chips.
                            ui.horizontal_wrapped(|ui| {
                                ui.label(
                                    RichText::new("SSTV")
                                        .size(12.0)
                                        .strong()
                                        .color(crate::theme::CYAN),
                                );
                                self.digi_freq_chip(ui, cmds);
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
                                sstv_level_bar(ui, signal);
                                if tx_active {
                                    ui.label(
                                        RichText::new(format!("● TX {:.0}%", progress * 100.0))
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
                                    let msg = if rifp {
                                        // RIFP only paints live from the raw
                                        // raster; anything else appears whole.
                                        "waiting for a picture…"
                                    } else if signal > 0.0008 {
                                        "waiting for a signal…"
                                    } else {
                                        "no / low audio"
                                    };
                                    ui.label(RichText::new(msg).size(11.0).weak());
                                }
                            });
                        });
                        // Draggable vertical divider between LIVE and RECEIVED.
                        let hresp =
                            crate::chrome::split_handle(ui, egui::vec2(handle_w, row_h), None);
                        if hresp.dragged() {
                            // Dragging right shrinks the gallery (grows LIVE).
                            let d = hresp.drag_delta().x / left_w.max(1.0);
                            self.view.sstv_gallery_fraction =
                                (self.view.sstv_gallery_fraction - d).clamp(0.2, 0.6);
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
            let hresp = crate::chrome::split_handle(ui, egui::vec2(handle_w, full_h), None);
            if hresp.dragged() {
                // Dragging right shrinks the TX column (grows the receive side).
                let d = hresp.drag_delta().x / avail.x.max(1.0);
                self.view.sstv_tx_fraction = (self.view.sstv_tx_fraction - d).clamp(0.22, 0.6);
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
                            if let Some(png) = self.sstv.compose_png(dims) {
                                cmds.push(if rifp {
                                    Command::RifpTx { png }
                                } else {
                                    Command::SstvTx { mode: self.sstv.tx_mode, png }
                                });
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
                        // RIFP knows where a picture came from and how it was
                        // carried; SSTV knows none of that, and says nothing.
                        if let Some(m) = &r.rifp {
                            ui.add_space(4.0);
                            let from = m.sender.as_deref().unwrap_or("unidentified");
                            ui.label(
                                RichText::new(format!(
                                    "{from} · {} · {}×{} {}-bit · {} / {} · {} octets in {} chunks \
                                     ({} first pass) · session {}",
                                    m.filename,
                                    m.width,
                                    m.height,
                                    m.bits_per_pixel,
                                    m.media_type,
                                    m.content_encoding,
                                    m.encoded_size,
                                    m.chunk_count,
                                    m.chunks_first_pass,
                                    m.session,
                                ))
                                .size(10.5)
                                .weak(),
                            );
                            if let Some(hint) = &m.hint {
                                ui.label(RichText::new(hint).size(11.0).italics());
                            }
                        }
                    });
            } else {
                open = false;
            }
            if !open {
                self.sstv.enlarged = None;
            }
        }
    }

    /// The RIFP half of the image panel's control strip: profile, picture size
    /// and encoding, robustness, the transfer readout, and the sessions being
    /// reassembled.
    fn rifp_controls(&mut self, ui: &mut egui::Ui, cmds: &mut Vec<Command>) {
        let st = self.sstv.rifp.clone();
        let seeded = self.digi_cfg_seeded;
        let mut changed = false;

        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("RIFP").size(12.0).strong().color(crate::theme::CYAN));
            // Outside the enabled scope: which frequency to sit on has nothing
            // to do with whether the operator's digi config has loaded yet.
            self.digi_freq_chip(ui, cmds);
            ui.add_enabled_ui(seeded, |ui| {
                for p in RifpProfile::ALL {
                    let active = self.digi_cfg_edit.rifp_profile == p;
                    if crate::chrome::chip(ui, active, p.label())
                        .on_hover_text(format!(
                            "{} — {:.0} baud CPFSK, ±{:.0} Hz, {:.0} kHz occupied bandwidth",
                            p.name(),
                            p.symbol_rate(),
                            p.deviation_hz(),
                            p.bandwidth_hz() / 1000.0,
                        ))
                        .clicked()
                        && !active
                    {
                        self.digi_cfg_edit.rifp_profile = p;
                        changed = true;
                    }
                }
                ui.separator();
                ui.label(RichText::new("Size").size(10.0).weak());
                for s in RifpSize::ALL {
                    let active = self.digi_cfg_edit.rifp_size == s;
                    if crate::chrome::chip(ui, active, s.label()).clicked() && !active {
                        self.digi_cfg_edit.rifp_size = s;
                        self.sstv.preview_dirty = true;
                        changed = true;
                    }
                }
            });
        });
        ui.add_space(4.0);

        // The bandwidth warning, and a jump to the calling frequency. RIFP
        // itself is band-agnostic; what is legal is not.
        let dial = self.state.rx_freq_hz();
        ui.horizontal_wrapped(|ui| {
            let profile = self.digi_cfg_edit.rifp_profile;
            if profile.fits_at(dial) {
                ui.label(
                    RichText::new(format!(
                        "{} · ~{:.0} kHz occupied · dial is the channel centre",
                        profile.name(),
                        profile.bandwidth_hz() / 1000.0,
                    ))
                    .size(10.5)
                    .weak(),
                );
            } else {
                ui.label(
                    RichText::new(format!(
                        "⚠ {} occupies ~{:.0} kHz — too wide for a narrow-band segment",
                        profile.name(),
                        profile.bandwidth_hz() / 1000.0,
                    ))
                    .size(10.5)
                    .strong()
                    .color(crate::theme::PINK),
                )
                .on_hover_text(format!(
                    "RIFP assigns no frequency, and sdroxide will transmit it wherever you tune. \
                     A {:.0} kHz channel only fits where wideband or FM operation is allowed — \
                     {} — and not in a narrow-band segment, least of all on HF. Even inside those \
                     your own licence conditions may be narrower. You are the operator; check \
                     your own rules.",
                    profile.bandwidth_hz() / 1000.0,
                    profile.wide_segments_text(),
                ));
            }
            if (dial - sdroxide_types::RIFP_CALLING_HZ).abs() > 1.0
                && crate::chrome::chip(ui, false, "433.920")
                    .on_hover_text("The calling frequency the draft names")
                    .clicked()
            {
                cmds.push(Command::SetVfo {
                    vfo: self.state.active_vfo,
                    hz: sdroxide_types::RIFP_CALLING_HZ,
                });
            }
        });
        ui.add_space(5.0);

        // Encoding and depth: what the picture is turned into before framing.
        ui.horizontal_wrapped(|ui| {
            ui.add_enabled_ui(seeded, |ui| {
                ui.label(RichText::new("Encode").size(10.0).weak()).on_hover_text(
                    "How the picture is encoded into the object RIFP carries. Auto tries each and \
                 sends the smallest.",
                );
                for e in RifpEncoding::TX_MENU {
                    let active = self.digi_cfg_edit.rifp_encoding == e;
                    let hover = match e.manifest_pair() {
                        Some((mt, ce)) => format!("{mt} / {ce}"),
                        None => "Try every encoding, send the smallest (never lossy)".into(),
                    };
                    if crate::chrome::chip(ui, active, e.label()).on_hover_text(hover).clicked()
                        && !active
                    {
                        self.digi_cfg_edit.rifp_encoding = e;
                        changed = true;
                    }
                }
                ui.separator();
                ui.label(RichText::new("Gray").size(10.0).weak()).on_hover_text(
                "Grayscale depth. RIFP's raster is grayscale by definition — colour has no place \
                 in its manifest.",
            );
                for bits in [1u8, 2, 4, 8] {
                    let active = self.digi_cfg_edit.rifp_bits_per_pixel == bits;
                    if crate::chrome::chip(ui, active, &format!("{bits}b")).clicked() && !active {
                        self.digi_cfg_edit.rifp_bits_per_pixel = bits;
                        changed = true;
                    }
                }
                let mut dither = self.digi_cfg_edit.rifp_dither;
                if crate::chrome::chip(ui, dither, "Dither")
                    .on_hover_text("Diffuse quantisation error — worth it below 8 bits")
                    .clicked()
                {
                    dither = !dither;
                    self.digi_cfg_edit.rifp_dither = dither;
                    changed = true;
                }
            });
        });
        ui.add_space(5.0);

        // Robustness: RIFP has no repair requests, so repetition is the only
        // recovery there is.
        ui.horizontal_wrapped(|ui| {
            ui.add_enabled_ui(seeded, |ui| {
                ui.label(RichText::new("Repeat data").size(10.0).weak()).on_hover_text(
                    "Send every data frame this many times. RIFP is one-way with no repair \
                     requests, so this is the only recovery a receiver gets.",
                );
                ui.spacing_mut().slider_width = 90.0;
                changed |= ui
                    .add(egui::Slider::new(&mut self.digi_cfg_edit.rifp_data_repeats, 1..=4))
                    .drag_stopped();
                ui.label(RichText::new("Chunk").size(10.0).weak())
                    .on_hover_text("Payload octets per data frame (the profile recommends 192)");
                changed |= ui
                    .add(
                        egui::Slider::new(&mut self.digi_cfg_edit.rifp_chunk_size, 32..=1024)
                            .step_by(16.0),
                    )
                    .drag_stopped();
            });
            ui.separator();
            if st.tx_active {
                ui.label(
                    RichText::new(format!(
                        "● TX frame {}/{} · {} s left",
                        st.tx_frame, st.tx_frames, st.tx_remaining_s
                    ))
                    .size(11.0)
                    .strong()
                    .color(crate::theme::PINK),
                );
            }
            if let Some(enc) = st.tx_encoding {
                ui.label(
                    RichText::new(format!("sent as {} · {} octets", enc.label(), st.tx_bytes))
                        .size(10.0)
                        .weak(),
                );
            }
        });
        ui.add_space(5.0);

        // Counters and the sessions being reassembled.
        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new(format!(
                    "frames {} · bad {} · pictures {}",
                    st.rx_frames, st.rx_bad_frames, st.rx_objects
                ))
                .size(10.0)
                .weak(),
            )
            .on_hover_text("Valid frames, frames that failed CRC, and complete verified pictures");
            if st.sessions.is_empty() {
                ui.label(RichText::new("no transfer in progress").size(10.0).weak());
            }
            for s in &st.sessions {
                ui.separator();
                let from = s.sender.as_deref().unwrap_or_else(|| shorten(&s.session, 8));
                let label = if s.total > 0 {
                    format!("{from} {}/{}", s.have, s.total)
                } else {
                    format!("{from} {}", s.have)
                };
                let colour =
                    if s.have_manifest { crate::theme::GREEN } else { crate::theme::YELLOW };
                ui.label(RichText::new(label).size(10.5).strong().color(colour)).on_hover_text(
                    if s.have_manifest {
                        format!("session {} · idle {} s", s.session, s.idle_s)
                    } else {
                        format!(
                            "session {} · chunks held, still waiting for the manifest · idle {} s",
                            s.session, s.idle_s
                        )
                    },
                );
                rifp_chunk_map(ui, s);
                if crate::chrome::chip(ui, false, "✕")
                    .on_hover_text("Forget this incomplete transfer")
                    .clicked()
                {
                    cmds.push(Command::RifpDropSession(s.session.clone()));
                }
            }
        });
        if let Some(err) = &st.last_error {
            ui.label(RichText::new(err).size(10.0).color(crate::theme::YELLOW));
        }
        if changed && seeded {
            cmds.push(Command::SetDigiConfig(self.digi_cfg_edit.clone()));
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
            self.sstv.received.push(SstvRecv { mode: None, rifp: None, tex });
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
                    let mut disp = egui::ColorImage::new([iw, ih], vec![Color32::BLACK; iw * ih]);
                    for (px, &v) in disp.pixels.iter_mut().zip(gray.iter()) {
                        *px = Color32::from_gray(v);
                    }
                    self.img_disp = Some(ctx.load_texture(
                        "rfpaint_img_disp",
                        disp,
                        egui::TextureOptions::LINEAR,
                    ));
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
    /// The weather-fax panel: the chart as it arrives, the controls that decide
    /// its geometry, and the gallery of ones already saved.
    ///
    /// Receive only, so there is no transmit half — the space goes to the
    /// picture instead, which is the whole point of the mode.
    fn wefax_panel(&mut self, ui: &mut egui::Ui, cmds: &mut Vec<Command>, panel_h: f32) {
        use crate::theme;

        let st = self.wefax.status;
        let ctx = ui.ctx().clone();
        self.wefax_load_disk_once(&ctx);
        // A chart arrives at two lines a second; there is no need to chase it
        // any faster than the eye can follow.
        ctx.request_repaint_after(Duration::from_millis(200));

        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("WEFAX").size(12.0).strong().color(theme::CYAN));
            self.wefax_station_chip(ui, cmds);

            // START / STOP. Starting by hand is the normal way in: a chart runs
            // for a quarter of an hour and you will almost always have tuned to
            // it after the start tone went by.
            let (face, hint) = if st.receiving {
                (" ■ STOP ", "End the chart now and save what has arrived")
            } else {
                (" ● START ", "Start a chart now, without waiting for a start tone")
            };
            if crate::chrome::chip_accent(
                ui,
                st.receiving,
                RichText::new(face).strong(),
                if st.receiving { theme::PINK } else { theme::GREEN },
                theme::INK_ON_CYAN,
            )
            .on_hover_text(hint)
            .clicked()
            {
                cmds.push(if st.receiving { Command::WefaxStop } else { Command::WefaxStart });
            }

            // What the receiver is making of the signal.
            let (text, colour) = if st.phasing {
                ("phasing…".to_string(), theme::YELLOW)
            } else if st.receiving {
                (format!("{} lines", st.lines), theme::GREEN)
            } else if self.wefax.has_live() {
                (format!("{} lines held", self.wefax.live_size().1), theme::CYAN_DIM)
            } else {
                ("listening".to_string(), theme::LINE_LIT)
            };
            ui.label(RichText::new(text).color(colour).size(11.0));

            // The tuning readout. A correctly tuned receiver puts the
            // subcarrier's excursions around 1900 Hz; several hundred hertz off
            // and the picture is all black or all white.
            let off = st.subcarrier_hz - 1900.0;
            ui.label(
                RichText::new(format!("{:+.0} Hz", off))
                    .color(if off.abs() < 120.0 { theme::GREEN } else { theme::YELLOW })
                    .size(11.0)
                    .monospace(),
            )
            .on_hover_text(
                "Subcarrier offset from 1900 Hz. Tune for roughly zero: the fax carrier is \
                 1500 Hz black to 2300 Hz white, and a receiver a few hundred hertz off clips \
                 the picture to solid black or solid white.",
            );
            self.digi_squelch_slider(ui, cmds);
        });

        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let seeded = self.digi_cfg_seeded;
            let mut changed = false;

            ui.label(RichText::new("LPM").color(theme::CYAN_DIM).size(9.5).strong());
            for l in sdroxide_types::WefaxLpm::ALL {
                let on = self.digi_cfg_edit.wefax_lpm == l;
                if crate::chrome::chip(ui, on, l.label()).clicked() && !on {
                    self.digi_cfg_edit.wefax_lpm = l;
                    changed = true;
                }
            }
            ui.add_space(8.0);
            ui.label(RichText::new("IOC").color(theme::CYAN_DIM).size(9.5).strong());
            for i in sdroxide_types::WefaxIoc::ALL {
                let on = self.digi_cfg_edit.wefax_ioc == i;
                if crate::chrome::chip(ui, on, i.value().to_string())
                    .on_hover_text(format!("{} pixels per line", i.width()))
                    .clicked()
                    && !on
                {
                    self.digi_cfg_edit.wefax_ioc = i;
                    changed = true;
                }
            }

            ui.add_space(8.0);
            let auto_start = self.digi_cfg_edit.wefax_auto_start;
            if crate::chrome::chip(ui, auto_start, "AUTO START")
                .on_hover_text(
                    "Begin a chart when the 300 Hz (IOC 576) or 675 Hz start tone is heard",
                )
                .clicked()
            {
                self.digi_cfg_edit.wefax_auto_start = !auto_start;
                changed = true;
            }
            let auto_stop = self.digi_cfg_edit.wefax_auto_stop;
            if crate::chrome::chip(ui, auto_stop, "AUTO STOP")
                .on_hover_text(
                    "End it on the 450 Hz stop tone. Turn off to keep recording through a \
                     station that sends several charts back to back.",
                )
                .clicked()
            {
                self.digi_cfg_edit.wefax_auto_stop = !auto_stop;
                changed = true;
            }

            ui.add_space(8.0);
            // Phase nudge: for a chart whose phasing pulse was missed, which is
            // every chart you tune into halfway through.
            ui.label(RichText::new("PHASE").color(theme::CYAN_DIM).size(9.5).strong());
            for (face, px) in [("⏪", -100), ("◀", -10), ("▶", 10), ("⏩", 100)] {
                if crate::chrome::chip(ui, false, face)
                    .on_hover_text(format!("Shift the picture {px} pixels"))
                    .clicked()
                {
                    cmds.push(Command::WefaxNudge(px));
                }
            }

            ui.add_space(8.0);
            ui.label(RichText::new("SLANT").color(theme::CYAN_DIM).size(9.5).strong());
            let mut ppm = self.digi_cfg_edit.wefax_slant_ppm;
            if ui
                .add(
                    egui::DragValue::new(&mut ppm)
                        .speed(0.5)
                        .range(-500.0..=500.0)
                        .suffix(" ppm")
                        .fixed_decimals(1),
                )
                .on_hover_text(
                    "Sample-clock trim. If the chart leans to the left, increase this; to the \
                     right, decrease it. A sound card a hundred ppm off walks a quarter-hour \
                     chart most of a line sideways.",
                )
                .changed()
            {
                self.digi_cfg_edit.wefax_slant_ppm = ppm;
                changed = true;
            }

            // How the chart in progress is displayed. Nothing here touches the
            // decoder — a chart takes a quarter of an hour, and waiting for it
            // to finish before being allowed to look at the top of it is the
            // single most irritating thing about receiving fax.
            ui.add_space(8.0);
            ui.label(RichText::new("VIEW").color(theme::CYAN_DIM).size(9.5).strong());
            use crate::wefax::Zoom;
            let zoom = self.wefax.zoom;
            for (face, z, hint) in [
                ("FIT", Zoom::FitWidth, "Scale the chart to the panel width"),
                ("WHOLE", Zoom::Whole, "Shrink the chart until all of it is in view at once"),
                ("50%", Zoom::Fixed(0.5), "Half size"),
                ("1:1", Zoom::Fixed(1.0), "One screen pixel per fax pixel — scroll for detail"),
                ("2×", Zoom::Fixed(2.0), "Twice size; scroll to move around the chart"),
            ] {
                if crate::chrome::chip(ui, zoom == z, face).on_hover_text(hint).clicked() {
                    self.wefax.zoom = z;
                }
            }

            // Vertical stretch. A chart comes out the wrong shape when the line
            // rate is not what the station is actually sending — 90 taken for
            // 120 makes it a third too tall — and this pulls it back while the
            // operator works out which rate that is.
            ui.label(RichText::new("HEIGHT").color(theme::CYAN_DIM).size(9.5).strong());
            let mut aspect = self.wefax.aspect;
            if ui
                .add(
                    egui::DragValue::new(&mut aspect)
                        .speed(0.01)
                        .range(crate::wefax::MIN_ASPECT..=crate::wefax::MAX_ASPECT)
                        .prefix("×")
                        .fixed_decimals(2),
                )
                .on_hover_text(
                    "Stretch the picture vertically. A chart that came out squashed or stretched \
                     is usually being decoded at the wrong line rate — this makes it readable, \
                     and the LPM chips fix it properly. Double-click to type a value.",
                )
                .changed()
            {
                self.wefax.aspect = aspect;
            }
            if (aspect - 1.0).abs() > 0.001
                && crate::chrome::chip(ui, false, "RESET")
                    .on_hover_text("Back to the picture's own proportions")
                    .clicked()
            {
                self.wefax.aspect = 1.0;
            }

            let follow = self.wefax.follow;
            if crate::chrome::chip(ui, follow, "FOLLOW")
                .on_hover_text(
                    "Keep the newest lines in view. Scrolling up turns this off so you can read \
                     what has already arrived; scrolling back to the bottom turns it on again.",
                )
                .clicked()
            {
                self.wefax.follow = !follow;
            }

            if changed && seeded {
                cmds.push(Command::SetDigiConfig(self.digi_cfg_edit.clone()));
            }
        });

        ui.add_space(6.0);

        // The picture. Everything left of the gallery strip, scrollable, with
        // the newest rows kept in view while a chart is being received.
        //
        // Height from what is actually left rather than a fixed subtraction, so
        // the control rows wrapping in a narrow window shortens the picture
        // instead of pushing it out of the panel.
        let avail_h = ui.available_height().max(80.0).min(panel_h);
        let handle_w = 7.0;
        ui.horizontal_top(|ui| {
            // The gallery takes a user-draggable fraction of the width, the
            // chart the rest; each keeps enough to stay useful. The strip is
            // worth widening to read the labels that tell a morning's
            // identical-looking surface analyses apart, and worth shrinking
            // back out of the way while a chart is being read at 1:1.
            let total_w = ui.available_width();
            let gap = ui.spacing().item_spacing.x;
            let gallery_w = (total_w * self.view.wefax_gallery_fraction)
                .clamp(120.0, (total_w - handle_w - 2.0 * gap - 200.0).max(120.0));
            let img_w = (total_w - gallery_w - handle_w - 2.0 * gap).max(120.0);
            // Both columns are explicitly top-down: `allocate_ui` inherits the
            // surrounding layout, which is the horizontal one that puts the two
            // columns side by side, and a gallery laid out left-to-right walks
            // its thumbnails off the edge of the window.
            ui.allocate_ui_with_layout(
                egui::vec2(img_w, avail_h),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    let receiving = st.receiving;
                    let (w, h) = self.wefax.live_size();
                    // Cloned rather than borrowed: the follow flag is updated from
                    // the scroll position afterwards, which needs the state back.
                    let tex = self.wefax.live_texture(&ctx).cloned();
                    match tex {
                        Some(tex) => {
                            // The scroll area's own width, less the bar it will
                            // put down the side, is what the picture has to fit
                            // into; using the column width would leave a chart
                            // fitted "to the width" a scrollbar too wide.
                            let bar = ui.spacing().scroll.bar_width + 4.0;
                            let view = (img_w - bar, avail_h - bar);
                            let size = crate::wefax::live_size(
                                self.wefax.zoom,
                                self.wefax.aspect,
                                view,
                                (w, h),
                            );
                            let out = egui::ScrollArea::both()
                                .id_salt("wefax-live")
                                .auto_shrink([false, false])
                                // Follow the newest rows only while the operator is
                                // at the bottom. Sticking regardless would snap the
                                // view back every time a line arrived, which is
                                // exactly what makes a chart unreadable until it has
                                // finished.
                                .stick_to_bottom(receiving && self.wefax.follow)
                                .show(ui, |ui| {
                                    ui.add(
                                        egui::Image::new(&tex)
                                            .fit_to_exact_size(egui::vec2(size.0, size.1))
                                            // The size above already carries the
                                            // aspect the operator asked for;
                                            // preserving the texture's own would
                                            // undo the stretch control.
                                            .maintain_aspect_ratio(false),
                                    );
                                });
                            // Where they left the view decides whether we keep
                            // following: at the bottom means "show me the new
                            // lines", anywhere else means "I am reading".
                            let slack = (out.content_size.y - out.inner_rect.height()).max(0.0);
                            self.wefax.follow = out.state.offset.y >= slack - 8.0;
                        }
                        None => {
                            ui.centered_and_justified(|ui| {
                                ui.label(
                                    RichText::new(
                                        "Tune a fax schedule in USB and wait for a start tone, or \
                                     press START to begin mid-chart.",
                                    )
                                    .color(theme::LINE_LIT)
                                    .size(11.5),
                                );
                            });
                        }
                    }
                },
            );

            // Draggable vertical divider between the chart and the gallery.
            let hresp = crate::chrome::split_handle(ui, egui::vec2(handle_w, avail_h), None);
            if hresp.dragged() {
                // Dragging right shrinks the gallery (grows the chart).
                let d = hresp.drag_delta().x / total_w.max(1.0);
                self.view.wefax_gallery_fraction =
                    (self.view.wefax_gallery_fraction - d).clamp(0.1, 0.5);
            }

            ui.allocate_ui_with_layout(
                egui::vec2(gallery_w, avail_h),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_max_width(gallery_w);
                    self.wefax_gallery(ui, gallery_w);
                },
            );
        });

        self.wefax_viewer(&ctx);
    }

    /// The gallery of saved charts: a thumbnail per chart, labelled with when it
    /// was received and which station it came from, and clickable to open it
    /// full size.
    ///
    /// The labels are the point. A quarter of a station's daily output is
    /// surface analyses that look identical at thumbnail size, and picking out
    /// "yesterday's 06Z from Pinneberg" without a date on it means opening them
    /// one at a time.
    fn wefax_gallery(&mut self, ui: &mut egui::Ui, width: f32) {
        use crate::theme;

        let dir = self.wefax.dir.clone();
        ui.horizontal(|ui| {
            ui.label(RichText::new("SAVED").color(theme::CYAN_DIM).size(9.5).strong());
            let n = self.wefax.gallery.len() + self.wefax.disk_extra;
            if n > 0 {
                ui.label(RichText::new(format!("{n}")).color(theme::LINE_LIT).size(9.5));
            }
            if !dir.is_empty()
                && crate::chrome::chip(ui, false, RichText::new("PATH").size(9.5))
                    .on_hover_text(format!("Charts are saved in\n{dir}\n\nClick to copy the path"))
                    .clicked()
            {
                ui.ctx().copy_text(dir.clone());
            }
        });

        if self.wefax.gallery.is_empty() {
            ui.label(
                RichText::new(if dir.is_empty() {
                    "Finished charts collect here.".to_string()
                } else {
                    format!("Finished charts are saved in {dir} and collect here.")
                })
                .color(theme::LINE_LIT)
                .size(10.0),
            );
            return;
        }

        let thumb_w = (width - 20.0).max(60.0);
        // A chart is about three times wider than it is tall, so a thumbnail
        // that keeps its shape is roughly a third of its width; charts cut short
        // are shorter still and simply take less room.
        let thumb_h = thumb_w * 0.36;
        let mut open = None;
        egui::ScrollArea::vertical().id_salt("wefax-gallery").auto_shrink([false, false]).show(
            ui,
            |ui| {
                for (i, c) in self.wefax.gallery.iter().enumerate() {
                    let selected = self.wefax.viewing == Some(i);
                    // The whole card — picture and label together — is the
                    // target, so a click near the date is not a miss.
                    let card = ui.scope_builder(
                        egui::UiBuilder::new().sense(egui::Sense::click()),
                        |ui| {
                            ui.set_width(thumb_w);
                            ui.add(
                                egui::Image::new(&c.texture)
                                    .fit_to_exact_size(egui::vec2(thumb_w, thumb_h))
                                    .maintain_aspect_ratio(true),
                            );
                            match c.meta {
                                Some(m) => {
                                    ui.label(
                                        RichText::new(m.when_label())
                                            .color(if selected { theme::CYAN } else { theme::TEXT })
                                            .size(10.0)
                                            .monospace(),
                                    );
                                    // A chart saved before the name carried a
                                    // frequency has nothing to say about where
                                    // it came from; its size is at least true.
                                    ui.label(
                                        RichText::new(m.where_label().unwrap_or_else(|| {
                                            format!("{} × {}", c.size.0, c.size.1)
                                        }))
                                        .color(theme::CYAN_DIM)
                                        .size(9.5),
                                    );
                                }
                                // Something in the directory that this program
                                // did not name: show what there is rather than
                                // inventing a date for it.
                                None => {
                                    ui.label(RichText::new(&c.name).color(theme::TEXT).size(9.5));
                                }
                            }
                        },
                    );
                    let resp = card.response;
                    if selected || resp.hovered() {
                        ui.painter().rect_stroke(
                            resp.rect.expand(2.0),
                            2.0,
                            egui::Stroke::new(
                                1.0,
                                if selected { theme::CYAN } else { theme::LINE_LIT },
                            ),
                            egui::StrokeKind::Inside,
                        );
                    }
                    if resp
                        .on_hover_text(format!(
                            "{}\n{} × {} pixels\n{}",
                            c.meta.map_or_else(|| c.name.clone(), |m| m.when_full()),
                            c.size.0,
                            c.size.1,
                            c.name
                        ))
                        .clicked()
                    {
                        open = Some(i);
                    }
                    ui.add_space(6.0);
                }
                // Charts beyond the ones held as textures are still on disk;
                // saying so stops the gallery looking like it lost them.
                if self.wefax.disk_extra > 0 {
                    ui.label(
                        RichText::new(format!("+{} older on disk", self.wefax.disk_extra))
                            .color(theme::LINE_LIT)
                            .size(9.5),
                    );
                }
            },
        );
        if open.is_some() {
            self.wefax.viewing = open;
        }
    }

    /// The radiofax schedules, as a chip that opens a station picker.
    ///
    /// These transmitters are not on any band plan and their frequencies are
    /// not the kind anyone remembers, so without this the mode starts with a
    /// trip to a web page. Picking one tunes the **dial**, which is 1.9 kHz
    /// below the published carrier — the subtraction that otherwise produces a
    /// blank page and no clue why.
    fn wefax_station_chip(&self, ui: &mut egui::Ui, cmds: &mut Vec<Command>) {
        use sdroxide_types::{WEFAX_STATIONS, WefaxStation};
        let dial = self.state.active_freq_hz();
        // The station we are on, if any, so the chip reads as a position.
        let here = WefaxStation::at_dial(dial);
        // No emoji in the face: the default fonts have no radio-mast glyph and
        // it comes out as an empty box.
        let face = match &here {
            Some((s, f)) => {
                format!("{} · {:.1}", s.name.split_whitespace().next().unwrap_or(""), f)
            }
            None => "STATIONS".to_string(),
        };
        let btn = crate::chrome::chip(ui, here.is_some(), RichText::new(face).size(11.0))
            .on_hover_text("Broadcast radiofax schedules — picking one tunes the dial");

        let mut pick = None;
        let resp = egui::Popup::from_toggle_button_response(&btn)
            .frame(crate::chrome::window_frame())
            .close_behavior(egui::PopupCloseBehavior::CloseOnClick)
            .show(|ui| {
                ui.set_max_width(420.0);
                for s in WEFAX_STATIONS {
                    ui.label(
                        RichText::new(s.name).color(crate::theme::CYAN_DIM).size(10.0).strong(),
                    );
                    ui.horizontal_wrapped(|ui| {
                        for &f in s.carriers_khz {
                            let d = WefaxStation::dial_hz(f);
                            let on = (d - dial).abs() < WefaxStation::NEAR_HZ;
                            if crate::chrome::chip(ui, on, format!("{f:.1}"))
                                .on_hover_text(format!(
                                    "Published carrier {f:.1} kHz → dial {:.1} kHz USB",
                                    d / 1000.0
                                ))
                                .clicked()
                            {
                                pick = Some(d);
                            }
                        }
                    });
                    ui.add_space(2.0);
                }
                ui.label(
                    RichText::new(
                        "Frequencies are the published carrier; the dial goes 1.9 kHz below it, \
                         which is done for you. Schedules change and stations close — treat this \
                         as where to start looking, not a timetable.",
                    )
                    .color(crate::theme::LINE_LIT)
                    .size(10.0),
                );
            });
        if let Some(r) = &resp {
            crate::chrome::paint_popup_cut_border(ui.ctx(), &r.response, 1.0);
        }
        if let Some(hz) = pick {
            cmds.push(Command::SetVfo { vfo: self.state.active_vfo, hz });
        }
    }

    /// A saved chart, full size, in its own window.
    ///
    /// A weather chart is unreadable at gallery size — the whole value of it is
    /// the fronts and the isobars — so opening one properly is not optional.
    /// Stepping between charts from inside the window matters for the same
    /// reason: comparing this run against the last one is most of what the
    /// charts are for.
    fn wefax_viewer(&mut self, ctx: &egui::Context) {
        let Some(i) = self.wefax.viewing else { return };
        let n = self.wefax.gallery.len();
        let Some(chart) = self.wefax.gallery.get(i) else {
            self.wefax.viewing = None;
            return;
        };
        let (name, size, tex) = (chart.name.clone(), chart.size, chart.texture.clone());
        let title = chart.title();
        let mut open = true;
        let mut step = 0i32;
        let resp = egui::Window::new(format!("{title}  ·  {}×{}", size.0, size.1))
            .id(egui::Id::new("wefax-viewer"))
            .open(&mut open)
            .frame(crate::chrome::window_frame())
            .default_size([900.0, 640.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    // Newer is up the list, older is down it.
                    if crate::chrome::chip(ui, false, "◀ NEWER")
                        .on_hover_text("The chart received after this one")
                        .clicked()
                    {
                        step = -1;
                    }
                    ui.label(
                        RichText::new(format!("{} of {n}", i + 1))
                            .color(crate::theme::CYAN_DIM)
                            .size(10.0),
                    );
                    if crate::chrome::chip(ui, false, "OLDER ▶")
                        .on_hover_text("The chart received before this one")
                        .clicked()
                    {
                        step = 1;
                    }
                    ui.add_space(8.0);
                    ui.label(RichText::new(&name).color(crate::theme::LINE_LIT).size(10.0))
                        .on_hover_text("The file this chart was saved as");
                });
                ui.add_space(4.0);
                egui::ScrollArea::both()
                    .id_salt("wefax-viewer-scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add(egui::Image::new(&tex).maintain_aspect_ratio(true));
                    });
            });
        if let Some(r) = &resp {
            crate::chrome::paint_window_border(ctx, &r.response);
        }
        if step != 0 && n > 0 {
            self.wefax.viewing = Some((i as i32 + step).clamp(0, n as i32 - 1) as usize);
        }
        if !open {
            self.wefax.viewing = None;
        }
    }

    /// Load previously saved charts into the gallery, once per session.
    ///
    /// Reads the legacy config-directory store as well as the pictures one, so
    /// a collection saved before charts moved to `<Pictures>/sdroxide/wefax`
    /// still shows up rather than looking as though it had been thrown away.
    #[cfg(not(target_arch = "wasm32"))]
    fn wefax_load_disk_once(&mut self, ctx: &egui::Context) {
        if self.wefax.loaded_disk {
            return;
        }
        self.wefax.loaded_disk = true;
        let Ok(dir) = sdroxide_config::wefax_rx_dir() else { return };
        self.wefax.dir = dir.display().to_string();

        // Newest first, and only the most recent few: a chart is two megapixels
        // as a texture, and a season of them would be gigabytes of VRAM.
        const KEEP: usize = 24;
        let mut files: Vec<(i64, std::path::PathBuf)> = Vec::new();
        let dirs = std::iter::once(dir).chain(sdroxide_config::wefax_legacy_rx_dir());
        for d in dirs {
            let Ok(entries) = std::fs::read_dir(&d) else { continue };
            files.extend(
                entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("png")))
                    .map(|p| {
                        // Order by the timestamp the name carries; a file this
                        // program did not write has none, and sorts oldest.
                        let when = p
                            .file_name()
                            .and_then(|n| n.to_str())
                            .and_then(sdroxide_types::WefaxChartMeta::from_file_name)
                            .map_or(0, |m| m.unix);
                        (when, p)
                    }),
            );
        }
        files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
        self.wefax.disk_extra = files.len().saturating_sub(KEEP);
        for (_, path) in files.iter().take(KEEP) {
            let Ok(bytes) = std::fs::read(path) else { continue };
            let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
            self.wefax.add_chart(ctx, &name, &bytes);
        }
        // `add_chart` prepends, so reading newest-first leaves the list oldest
        // first; sorting puts it back the way the gallery is browsed.
        self.wefax.sort_gallery();
        // Nothing is open yet — `add_chart` shifted the viewer index for each
        // chart it prepended, and there was no chart to shift.
        self.wefax.viewing = None;
    }

    /// The browser tab has no config directory to read a gallery out of; the
    /// charts it receives this session are all it shows.
    #[cfg(target_arch = "wasm32")]
    fn wefax_load_disk_once(&mut self, _ctx: &egui::Context) {
        self.wefax.loaded_disk = true;
    }

    fn rade_panel(&mut self, ui: &mut egui::Ui, cmds: &mut Vec<Command>, _panel_h: f32) {
        let status = self.digi_status.clone();
        let rade = status.as_ref().and_then(|s| s.rade).unwrap_or_default();
        let transmitting = status.as_ref().map(|s| s.transmitting).unwrap_or(false);

        ui.horizontal(|ui| {
            ui.label(RichText::new("RADE").size(11.0).strong().color(crate::theme::CYAN));
            ui.label(
                RichText::new("FreeDV V1 digital voice").size(10.5).color(crate::theme::CYAN_DIM),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if transmitting {
                    ui.label(RichText::new("● TX").size(11.0).strong().color(crate::theme::PINK));
                    ui.add_space(8.0);
                }
                // Silence the raw signal, leaving only decoded speech audible.
                let muted = self.digi_cfg_edit.rade_mute_analog;
                let resp = crate::chrome::chip(ui, muted, RichText::new("MUTE ANALOG").size(10.5));
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
            let (lamp, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
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
                .color(if lit {
                    crate::theme::GREEN
                } else {
                    Color32::from_gray(130)
                }),
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
        if let Some(path) =
            rfd::FileDialog::new().add_filter("Image", &["png", "jpg", "jpeg"]).pick_file()
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
/// First `n` characters of `text` — a short label that cannot panic on a
/// string shorter than it expects.
fn shorten(text: &str, n: usize) -> &str {
    match text.char_indices().nth(n) {
        Some((end, _)) => &text[..end],
        None => text,
    }
}

/// One incoming RIFP transfer's chunk map: a lit cell per chunk received, dark
/// per chunk still missing. Beyond what fits, it degrades to a plain bar — the
/// point is to see *where* the holes are, and a thousand one-pixel cells show
/// nothing.
fn rifp_chunk_map(ui: &mut egui::Ui, session: &sdroxide_types::RifpSession) {
    let cells = session.total.max(session.have) as usize;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(120.0, 10.0), egui::Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 2.0, Color32::from_gray(20));
    let have = |i: usize| session.map.get(i / 8).is_some_and(|b| b >> (i % 8) & 1 != 0);
    if cells > 0 && cells <= rect.width() as usize {
        let cw = rect.width() / cells as f32;
        for i in 0..cells {
            if !have(i) {
                continue;
            }
            let x = rect.left() + i as f32 * cw;
            p.rect_filled(
                egui::Rect::from_min_size(egui::pos2(x, rect.top()), egui::vec2(cw.max(1.0), 10.0)),
                0.0,
                crate::theme::GREEN,
            );
        }
    } else if session.total > 0 {
        let mut fill = rect;
        fill.set_width(rect.width() * (session.have as f32 / session.total as f32).clamp(0.0, 1.0));
        p.rect_filled(fill, 2.0, crate::theme::GREEN);
    }
    p.rect_stroke(
        rect,
        2.0,
        egui::Stroke::new(1.0, Color32::from_gray(60)),
        egui::StrokeKind::Inside,
    );
    resp.on_hover_text("Chunks received (lit) and still missing (dark)");
}

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

/// Roughly how many JS8 frames a message will take.
///
/// The panel shows this before the operator presses send, because JS8's most
/// surprising property to someone new is that a sentence can occupy a minute of
/// air time. It is an estimate: the real encoder chooses per frame between
/// Huffman and dictionary compression, so the true count is often lower. Being
/// pessimistic here is the right way round — a message that finishes early is a
/// pleasant surprise, one that runs long is not.
fn js8_frame_estimate(text: &str) -> u8 {
    const PER_FRAME: usize = 13;
    let n = text.trim().len();
    (n.div_ceil(PER_FRAME).max(1)).min(255) as u8
}

/// How long a JS8 station stays lit on the maps after it was last heard.
///
/// The mode's own convention is a heartbeat every ten or fifteen minutes, so
/// FT8's two-minute fade would leave the map blank between them.
const JS8_STATION_FADE_S: f64 = 900.0;

/// Least time between two locator lookups driven by the JS8 heard list.
const JS8_LOOKUP_INTERVAL_S: f64 = 1.5;

/// True when a message was aimed at *us* — our callsign, or a group we joined —
/// as opposed to at the whole band.
///
/// `@ALLCALL` reaches us and the assembler marks it `to_me` accordingly, but a
/// heartbeat is not addressed to anyone in particular: colouring every one of
/// them gold would leave nothing for a real call to stand out against.
fn js8_personally_addressed(m: &sdroxide_types::Js8Msg) -> bool {
    m.to_me && !m.to.eq_ignore_ascii_case("@ALLCALL")
}

/// What the composer can quote about our own station when it drafts a reply.
struct Js8Me {
    grid: String,
    status: String,
    /// Callsigns heard recently, most recent first — the answer to `HEARING?`.
    hearing: Vec<String>,
    /// The last thing we transmitted, which is what `AGN?` is asking for.
    last_sent: String,
}

/// One heard station as a [`Decode`], so the FT8 hover card can describe it
/// without learning a second station type.
fn js8_station_decode(
    h: &sdroxide_types::Js8Heard,
    grid: Option<String>,
    msg: Option<&sdroxide_types::Js8Msg>,
) -> Decode {
    Decode {
        slot_utc: h.last_utc,
        snr_db: h.snr_db,
        dt: 0.0,
        audio_hz: h.audio_hz,
        message: msg.map(js8_msg_summary).unwrap_or_default(),
        to: msg.map(|m| m.to.clone()).filter(|t| !t.is_empty()),
        from: Some(h.call.clone()),
        grid,
        is_cq: msg.is_some_and(|m| m.cmd.as_deref() == Some("CQ")),
        cq_to: None,
        rr73_to: None,
        free_text: false,
    }
}

/// A heard station's last transmission on one line: the command, then the text.
fn js8_msg_summary(m: &sdroxide_types::Js8Msg) -> String {
    let mut s = String::new();
    if let Some(c) = &m.cmd {
        s.push_str(c);
    }
    let text = m.text.trim();
    if !text.is_empty() {
        if !s.is_empty() {
            s.push(' ');
        }
        s.push_str(text);
    }
    if !m.complete {
        s.push('…');
    }
    s
}

fn non_empty(s: &str) -> Option<&str> {
    let t = s.trim();
    (!t.is_empty()).then_some(t)
}

/// The reply a standard JS8 exchange expects to this message, if there is one.
///
/// JS8 carries a conversation, so this only ever *offers*: it fills the
/// composer and the operator is free to rewrite it before pressing send. What
/// it encodes is the handful of turns that are the same in every contact — a
/// heartbeat or a CQ is asking "can anyone hear me?" and wants a report back, a
/// question wants its answer, `HW CPY?` wants a report — so the routine part is
/// one click and everything else is still a text box.
///
/// `None` means "nothing standard to say", which is the answer for free text
/// and therefore for most of a rag-chew. The caller still selects the station,
/// so the composer is aimed at them with nothing typed in it.
fn js8_reply_for(msg: &sdroxide_types::Js8Msg, me: &Js8Me) -> Option<String> {
    let snr = msg.snr_db;
    Some(match msg.cmd.as_deref()? {
        // A heartbeat is answered with a heartbeat report, which is a distinct
        // command from a plain report: it says "this is an answer to your
        // beacon", not "we are in a QSO".
        "HB" => format!("HEARTBEAT SNR {snr}"),
        "CQ" | "SNR?" | "HW CPY?" | "HEARTBEAT SNR" => format!("SNR {snr}"),
        "GRID?" => format!("GRID {}", non_empty(&me.grid)?),
        "STATUS?" | "INFO?" => format!("STATUS {}", non_empty(&me.status)?),
        "HEARING?" => format!("HEARING {}", non_empty(&me.hearing.join(" "))?),
        // They answered us. Acknowledge, and from here it is a conversation.
        "SNR" | "GRID" | "STATUS" | "INFO" | "HEARING" | "FB" | "ACK" => "RR".into(),
        "QSL?" => "QSL".into(),
        "QSL" | "RR" => "73".into(),
        "73" | "SK" => "73".into(),
        // "Say again" wants the same words back, not a new sentence.
        "AGN?" => non_empty(&me.last_sent)?.to_string(),
        _ => return None,
    })
}

#[cfg(test)]
mod js8_panel_tests {
    use super::js8_frame_estimate;

    #[test]
    fn short_messages_take_one_frame() {
        assert_eq!(js8_frame_estimate("HELLO"), 1);
        assert_eq!(js8_frame_estimate("HI"), 1);
    }

    #[test]
    fn the_estimate_grows_with_the_message() {
        let short = js8_frame_estimate("HELLO WORLD");
        let long = js8_frame_estimate(
            "HELLO WORLD THIS IS A CONSIDERABLY LONGER MESSAGE THAT WILL SPAN SEVERAL FRAMES",
        );
        assert!(long > short, "{long} should exceed {short}");
    }

    #[test]
    fn an_empty_message_still_reads_as_one_frame_not_zero() {
        // The label is only shown for non-empty input, but a zero here would
        // render "0f · 0s" if that ever changed.
        assert_eq!(js8_frame_estimate(""), 1);
        assert_eq!(js8_frame_estimate("   "), 1);
    }

    use super::{Js8Me, js8_msg_summary, js8_personally_addressed, js8_reply_for};
    use sdroxide_types::Js8Msg;

    fn me() -> Js8Me {
        Js8Me {
            grid: "FN42".into(),
            status: "PORTABLE".into(),
            hearing: vec!["KN4CRD".into(), "VK3ABC".into()],
            last_sent: "KN4CRD HELLO FROM THE HILLS".into(),
        }
    }

    fn msg(cmd: Option<&str>, to: &str) -> Js8Msg {
        Js8Msg {
            from: "KN4CRD".into(),
            to: to.into(),
            text: String::new(),
            cmd: cmd.map(str::to_string),
            snr_db: -12,
            audio_hz: 1500.0,
            first_slot_utc: 1000,
            last_slot_utc: 1000,
            frames: 1,
            complete: true,
            to_me: true,
        }
    }

    #[test]
    fn an_announcement_drafts_the_report_it_is_asking_for() {
        // A heartbeat and a CQ are both "can anyone hear me?"; JS8's answer is
        // a signal report, and a heartbeat gets the report command that says
        // "this answers your beacon".
        assert_eq!(
            js8_reply_for(&msg(Some("HB"), "@ALLCALL"), &me()).as_deref(),
            Some("HEARTBEAT SNR -12")
        );
        assert_eq!(js8_reply_for(&msg(Some("CQ"), "@ALLCALL"), &me()).as_deref(), Some("SNR -12"));
        assert_eq!(
            js8_reply_for(&msg(Some("HW CPY?"), "N0JDS"), &me()).as_deref(),
            Some("SNR -12")
        );
    }

    #[test]
    fn a_question_drafts_its_answer() {
        for (cmd, want) in [
            ("SNR?", "SNR -12"),
            ("GRID?", "GRID FN42"),
            ("STATUS?", "STATUS PORTABLE"),
            ("HEARING?", "HEARING KN4CRD VK3ABC"),
            // "Say again" wants the same words back, not a new sentence.
            ("AGN?", "KN4CRD HELLO FROM THE HILLS"),
        ] {
            assert_eq!(
                js8_reply_for(&msg(Some(cmd), "N0JDS"), &me()).as_deref(),
                Some(want),
                "{cmd}"
            );
        }
    }

    #[test]
    fn a_contact_winds_itself_down() {
        for (cmd, want) in
            [("SNR", "RR"), ("QSL?", "QSL"), ("RR", "73"), ("73", "73"), ("SK", "73")]
        {
            assert_eq!(
                js8_reply_for(&msg(Some(cmd), "N0JDS"), &me()).as_deref(),
                Some(want),
                "{cmd}"
            );
        }
    }

    #[test]
    fn free_text_drafts_nothing_so_the_composer_is_left_alone() {
        // The point of JS8 is the rag-chew: there is no standard answer to
        // "GOOD MORNING FROM VIENNA", and guessing one would be in the way.
        assert_eq!(js8_reply_for(&msg(None, "N0JDS"), &me()), None);
        // Nor to traffic this station deliberately does not handle.
        for cmd in [">", "MSG TO:", "QUERY MSGS", "YES", "NO"] {
            assert_eq!(js8_reply_for(&msg(Some(cmd), "N0JDS"), &me()), None, "{cmd}");
        }
    }

    #[test]
    fn a_draft_is_dropped_rather_than_sent_empty() {
        // "GRID" with no grid says "I am here" and answers nothing, at the cost
        // of a full transmission.
        let blank = Js8Me { grid: String::new(), status: String::new(), ..me() };
        assert_eq!(js8_reply_for(&msg(Some("GRID?"), "N0JDS"), &blank), None);
        assert_eq!(js8_reply_for(&msg(Some("STATUS?"), "N0JDS"), &blank), None);
        let deaf = Js8Me { hearing: Vec::new(), last_sent: String::new(), ..me() };
        assert_eq!(js8_reply_for(&msg(Some("HEARING?"), "N0JDS"), &deaf), None);
        assert_eq!(js8_reply_for(&msg(Some("AGN?"), "N0JDS"), &deaf), None);
    }

    #[test]
    fn a_broadcast_is_not_a_message_addressed_to_us() {
        // Every heartbeat on the band is `to_me`; colouring them all gold would
        // leave nothing for a station actually calling us to stand out against.
        assert!(!js8_personally_addressed(&msg(Some("HB"), "@ALLCALL")));
        assert!(js8_personally_addressed(&msg(Some("SNR?"), "N0JDS")));
        assert!(js8_personally_addressed(&msg(Some("STATUS?"), "@JS8NET")));
    }

    #[test]
    fn a_stations_last_word_reads_as_one_line() {
        let mut m = msg(Some("HB"), "@ALLCALL");
        m.text = "EM73".into();
        assert_eq!(js8_msg_summary(&m), "HB EM73");
        m.cmd = None;
        assert_eq!(js8_msg_summary(&m), "EM73");
        // Still arriving, and the row has to say so.
        m.complete = false;
        assert_eq!(js8_msg_summary(&m), "EM73…");
    }
}

impl SdroxideApp {
    /// The slot length of the current mode, in seconds.
    ///
    /// FT8 and FT4 have theirs fixed by the mode; JS8's is an operator setting,
    /// so it has to come from the engine's status. The decode list groups rows
    /// into turns by this, and getting it wrong for JS8 Turbo would draw one
    /// "EVEN/ODD" header per two and a half turns.
    fn slot_period_s(&self) -> f64 {
        match self.state.rx[0].mode {
            Mode::Ft4 => 7.5,
            Mode::Js8 => self
                .digi_status
                .as_ref()
                .and_then(|s| s.js8.as_ref())
                .map_or(15.0, |j| j.speed.slot_s()),
            _ => 15.0,
        }
    }
}
