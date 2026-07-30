//! Core domain vocabulary shared by every sdroxide component, native and WASM.
//!
//! This crate must stay free of I/O, threads, and native-only dependencies:
//! it compiles for `wasm32-unknown-unknown`.

mod awards;
mod band;
mod band_segments;
pub mod broadcast;
mod callsign;
mod caps;
mod command;
mod contacts;
mod controller;
mod digi;
mod entity;
mod geo;
mod input;
mod js8;
mod memory;
mod meters;
mod mode;
mod netcfg;
mod radio;
mod rifp;
mod rigctld;
mod satcfg;
mod skimmer;
mod spectrum;
mod spot;
mod sstv;
mod state;
mod tciserver;
mod ui;
mod voice;
mod wefax;
mod worldmask;
mod wsjtx;

pub use awards::{
    Awards, Coverage, EntitySlot, Highlight, LogIndex, Novelty, Status as AwardStatus, US_STATES,
    compute_awards, counts, coverage_counts, entity_coverage, entity_name,
};
pub use band::Band;
pub use band_segments::{
    DigiChannel, FSQ_DIALS, FT4_DIALS, FT8_DIALS, FT8_DXPED_DIALS, JS8_DIALS, PSK_DIALS,
    PSK_RANGES, RIFP_CALLING, RTTY_DIALS, RTTY_RANGES, SSTV_CALLING, Segment, SegmentKind,
    WSPR_DIALS, digi_channels, digi_channels_in, is_auto_digi, is_cw_segment, is_digi_segment,
    is_psk_segment, is_rtty_segment, segment_kind_at,
};
pub use broadcast::{BroadcastStation, BroadcastStations};
pub use callsign::{CallsignInfo, UploadResult, UploadTarget};
pub use caps::{DeviceCaps, Direction, GainElement};
pub use command::Command;
pub use contacts::FsqContact;
pub use controller::{AudioDevices, RadioController, RadioEvent};
pub use digi::{
    ClockHealth, Decode, DigiConfig, DigiStatus, DxpedMode, FOX_MAX_SLOTS, FOX_ZONE_MAX_HZ,
    FoxCaller, FsqMsg, HOUND_ZONE_MAX_HZ, HellVariant, QsoRecord, QsoStep, QueuedCall, RadeStatus,
    ThorMode, TranscriptLine, adif_band, adif_to_qso_log, clock_health, cq_is_for_us, fmt_report,
    qso_log_to_adif, qso_log_to_text, utc_ymd_hms, worked_before, ymd_hms_to_unix,
};
pub use entity::{EntityInfo, EntityPlace, all_entities, resolve_callsign, resolve_prefix};
pub use geo::{
    distance_km, great_circle_points, grid_bearing, grid_distance_km, grid_to_latlon, is_land,
    land_cell, land_mask_dims,
};
pub use input::{
    Action, ActionInput, ActionKind, BindingTuning, ButtonMode, InputSettings, KeyBinding,
    KeyChord, MidiBinding, MidiMsg, MidiMsgKind, MidiSettings, MouseButton, MouseButtonBinding,
    RelativeMode, WheelAction, WheelSettings,
};
pub use js8::{
    HB_BAND_HI_HZ, HB_BAND_LO_HZ, HB_SLOT_HZ, Js8FrameInfo, Js8FrameKind, Js8Heard, Js8Msg,
    Js8Speed, Js8Status,
};
pub use memory::{BandStackEntry, MemoryChannel};
pub use meters::{Meters, TxMeters, TxTelemetry};
pub use mode::{AgcMode, Mode, NrLevel};
pub use netcfg::{
    ClusterConfig, Credentials, FeedConfig, FreeDvReporterConfig, LookupProvider, NetworkConfig,
    PskConfig,
};
pub use radio::{
    Backend, CatConfig, CatFamily, DigiMode, FlexConfig, FlexDevice, HpsdrConfig, HpsdrDevice,
    HpsdrFilterBoard, IcomConfig, LineState, ModeControl, Parity, PttMethod, RadioConfig,
    RtlSdrAgc, RtlSdrConfig, RtlSdrDevice, RtlSdrHfMode, SerialConfig, SoundFormat, StopBits,
    TciConfig,
};
pub use rifp::{
    RIFP_CALLING_HZ, RIFP_MAP_MAX_CHUNKS, RifpEncoding, RifpMeta, RifpProfile, RifpSession,
    RifpSize, RifpStatus,
};
pub use rigctld::RigctldConfig;
pub use satcfg::{
    CELESTRAK_GROUPS, CelestrakGroup, CustomTle, OrbitRings, Passband, SatConfig, SatFreqs,
    SatLink, TleSubscription, fmt_mhz as fmt_sat_mhz, parse_tle_block,
};
pub use skimmer::{SkimmerKind, SkimmerSettings, SkimmerSpot};
pub use spectrum::{SpectrumConfig, SpectrumFrame};
pub use spot::{Spot, SpotKind};
pub use sstv::{SstvMode, SstvStatus};
pub use state::{AtuState, OffsetState, RadioState, RxId, RxState, SQUELCH_OPEN_DB, TxState, Vfo};
pub use tciserver::TciServerConfig;
pub use ui::{Speed, UiSettings};
pub use voice::{VOICE_MAX_LEN_S, VOICE_SLOTS, VoiceSlotInfo, VoiceStatus, slot_label};
pub use wefax::{WEFAX_STATIONS, WefaxChartMeta, WefaxIoc, WefaxLpm, WefaxStation, WefaxStatus};
pub use wsjtx::WsjtxConfig;
