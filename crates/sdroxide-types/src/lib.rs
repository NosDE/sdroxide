//! Core domain vocabulary shared by every sdroxide component, native and WASM.
//!
//! This crate must stay free of I/O, threads, and native-only dependencies:
//! it compiles for `wasm32-unknown-unknown`.

mod awards;
mod band;
mod band_segments;
mod callsign;
mod caps;
mod command;
mod contacts;
mod controller;
mod digi;
mod entity;
mod geo;
mod input;
mod worldmask;
mod memory;
mod meters;
mod mode;
mod netcfg;
mod radio;
mod rigctld;
mod skimmer;
mod spectrum;
mod spot;
mod sstv;
mod state;
mod tciserver;
mod voice;
mod wsjtx;
mod ui;

pub use awards::{
    Awards, Highlight, LogIndex, Novelty, Status as AwardStatus, US_STATES, compute_awards, counts,
    entity_name,
};
pub use band::Band;
pub use callsign::{CallsignInfo, UploadResult, UploadTarget};
pub use entity::{EntityInfo, resolve_callsign};
pub use band_segments::{
    FT4_DIALS, FT8_DIALS, JS8_DIALS, PSK_RANGES, RTTY_RANGES, SSTV_CALLING, Segment, SegmentKind,
    WSPR_DIALS, is_auto_digi, is_cw_segment, is_digi_segment, is_psk_segment, is_rtty_segment,
    segment_kind_at,
};
pub use caps::{DeviceCaps, Direction, GainElement};
pub use contacts::FsqContact;
pub use command::Command;
pub use controller::{AudioDevices, RadioController, RadioEvent};
pub use digi::{
    Decode, DigiConfig, DigiStatus, FsqMsg, QsoRecord, QsoStep, RadeStatus, ThorMode,
    TranscriptLine, adif_band, adif_to_qso_log, cq_is_for_us, fmt_report, qso_log_to_adif,
    qso_log_to_text, utc_ymd_hms, worked_before, ymd_hms_to_unix,
};
pub use geo::{
    distance_km, grid_bearing, grid_distance_km, grid_to_latlon, great_circle_points, is_land,
    land_cell, land_mask_dims,
};
pub use input::{
    Action, ActionInput, ActionKind, BindingTuning, ButtonMode, InputSettings, KeyBinding, KeyChord,
    MidiBinding, MidiMsg, MidiMsgKind, MidiSettings, MouseButton, MouseButtonBinding, RelativeMode,
    WheelAction, WheelSettings,
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
    IcomConfig, LineState, ModeControl, Parity, PttMethod, RadioConfig, SerialConfig, SoundFormat,
    StopBits, TciConfig,
};
pub use rigctld::RigctldConfig;
pub use wsjtx::WsjtxConfig;
pub use skimmer::{SkimmerKind, SkimmerSettings, SkimmerSpot};
pub use spectrum::{SpectrumConfig, SpectrumFrame};
pub use spot::{Spot, SpotKind};
pub use sstv::{SstvMode, SstvStatus};
pub use state::{
    AtuState, OffsetState, RadioState, RxId, RxState, SQUELCH_OPEN_DB, TxState, Vfo,
};
pub use tciserver::TciServerConfig;
pub use ui::{Speed, UiSettings};
pub use voice::{VOICE_MAX_LEN_S, VOICE_SLOTS, VoiceSlotInfo, VoiceStatus, slot_label};
