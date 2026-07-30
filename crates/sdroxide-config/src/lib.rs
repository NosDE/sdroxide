//! Settings and radio-data persistence under the user config directory
//! (`~/.config/sdroxide/` on Linux).

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("no home/config directory available")]
    NoConfigDir,
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("serialize: {0}")]
    Serialize(#[from] toml::ser::Error),
}

/// User settings (`config.toml`). Everything has a default so a missing or
/// partial file always loads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// SoapySDR device args, e.g. "driver=hackrf". Empty = first device found.
    pub device_args: String,
    /// Preferred hardware sample rate in Hz.
    pub sample_rate: f64,
    /// dB offset applied to convert dBFS to dBm for the S-meter.
    pub cal_offset_db: f64,
    pub spectrum_fft: u32,
    pub spectrum_fps: u8,
    /// Server mode bind address.
    pub server_bind: String,
    pub server_port: u16,
    /// Refuse to transmit outside amateur bands.
    pub tx_ham_only: bool,
    /// Preferred audio output device name; `None` = system default.
    pub audio_output: Option<String>,
    /// Preferred audio input (microphone) device name; `None` = system default.
    pub audio_input: Option<String>,
    /// UI / display preferences (frame rate, waterfall + spectrum speed).
    pub ui: sdroxide_types::UiSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            device_args: String::new(),
            sample_rate: 1_536_000.0,
            cal_offset_db: 0.0,
            spectrum_fft: 4096,
            spectrum_fps: 30,
            server_bind: "0.0.0.0".into(),
            server_port: 4950,
            tx_ham_only: true,
            audio_output: None,
            audio_input: None,
            ui: sdroxide_types::UiSettings::default(),
        }
    }
}

/// Load just the UI/display preferences (frame rate, waterfall + spectrum speed).
pub fn load_ui_settings() -> sdroxide_types::UiSettings {
    Settings::load().ui
}

/// Persist the UI/display preferences, preserving every other setting
/// (read-modify-write so a concurrent edit elsewhere isn't clobbered).
pub fn save_ui_settings(ui: &sdroxide_types::UiSettings) -> Result<(), ConfigError> {
    let mut s = Settings::load();
    s.ui = *ui;
    s.save()
}

pub fn config_dir() -> Result<PathBuf, ConfigError> {
    directories::ProjectDirs::from("org", "sdroxide", "sdroxide")
        .map(|d| d.config_dir().to_path_buf())
        .ok_or(ConfigError::NoConfigDir)
}

/// Directory for received SSTV images (`~/.config/sdroxide/sstv_rx`), created
/// on demand.
pub fn sstv_rx_dir() -> Result<PathBuf, ConfigError> {
    let dir = config_dir()?.join("sstv_rx");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Directory for a mode's received pictures (`~/.config/sdroxide/<kind>_rx`),
/// created on demand.
///
/// One store per mode rather than one for everything: an SSTV picture and a
/// weather chart are browsed for entirely different reasons, and a
/// fifteen-minute chart arriving every half hour would bury a session's SSTV.
pub fn image_rx_dir(kind: &str) -> Result<PathBuf, ConfigError> {
    // The caller's `kind` is a literal today, but it ends up in a path.
    let safe: String = kind.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    let dir = config_dir()?.join(format!("{}_rx", if safe.is_empty() { "image" } else { &safe }));
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Directory for received weather-fax charts, created on demand: the user's
/// pictures directory (`<Pictures>/sdroxide/wefax`), or the config directory
/// (`~/.config/sdroxide/wefax_rx`) when the platform exposes no pictures folder.
///
/// Charts go where pictures go, unlike every other store here, because that is
/// what they are for. A weather chart is printed, mailed, dropped into a
/// passage plan or opened next to a routing program — all of which happen
/// outside this program, in a file manager, and none of which anyone will do
/// from a hidden directory under `~/.config`.
pub fn wefax_rx_dir() -> Result<PathBuf, ConfigError> {
    let dir = match directories::UserDirs::new()
        .and_then(|u| u.picture_dir().map(std::path::Path::to_path_buf))
    {
        Some(pictures) => pictures.join("sdroxide").join("wefax"),
        None => config_dir()?.join("wefax_rx"),
    };
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Where charts were kept before they moved to the pictures directory.
///
/// Read-only and never created: the gallery lists it alongside the current
/// store so an existing collection does not appear to have been lost. `None`
/// when it is the current store anyway, or when there is no config directory.
pub fn wefax_legacy_rx_dir() -> Option<PathBuf> {
    let old = config_dir().ok()?.join("wefax_rx");
    let current = wefax_rx_dir().ok()?;
    (old != current && old.is_dir()).then_some(old)
}

/// Directory for the operator's transmit-image slots
/// (`~/.config/sdroxide/sstv_tx`), created on demand.
pub fn sstv_tx_dir() -> Result<PathBuf, ConfigError> {
    let dir = config_dir()?.join("sstv_tx");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Directory for the voice keyer's recorded messages
/// (`~/.config/sdroxide/voice`), created on demand. One 48 kHz mono WAV per
/// slot, so a message can be edited or replaced with any audio editor.
pub fn voice_dir() -> Result<PathBuf, ConfigError> {
    let dir = config_dir()?.join("voice");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Directory for cached solar imagery and space-weather JSON
/// (`~/.config/sdroxide/solar`), created on demand.
///
/// The 3D solar view loads this before its first network request, so the window
/// opens with the last-known data and stays useful with no connection at all.
pub fn solar_cache_dir() -> Result<PathBuf, ConfigError> {
    let dir = config_dir()?.join("solar");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Directory for audio recordings, created on demand: the user's music/audio
/// directory (`<Music>/sdroxide`), or the config directory
/// (`~/.config/sdroxide/recordings`) when the platform exposes no music folder.
pub fn recordings_dir() -> Result<PathBuf, ConfigError> {
    let dir = match directories::UserDirs::new()
        .and_then(|u| u.audio_dir().map(std::path::Path::to_path_buf))
    {
        Some(music) => music.join("sdroxide"),
        None => config_dir()?.join("recordings"),
    };
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

impl Settings {
    /// Load settings; missing file or unreadable content falls back to
    /// defaults (with a warning), so startup never fails on config.
    pub fn load() -> Settings {
        let path = match config_dir() {
            Ok(d) => d.join("config.toml"),
            Err(e) => {
                warn!("no config dir: {e}; using default settings");
                return Settings::default();
            }
        };
        match fs::read_to_string(&path) {
            Ok(text) => match toml::from_str(&text) {
                Ok(s) => s,
                Err(e) => {
                    warn!("failed to parse {}: {e}; using defaults", path.display());
                    Settings::default()
                }
            },
            Err(_) => Settings::default(),
        }
    }

    pub fn save(&self) -> Result<(), ConfigError> {
        let dir = config_dir()?;
        fs::create_dir_all(&dir)?;
        let text = toml::to_string_pretty(self)?;
        fs::write(dir.join("config.toml"), text)?;
        Ok(())
    }
}

/// Band-stack registers: up to 3 remembered (freq, mode, filter) per band.
pub type BandStacks =
    std::collections::HashMap<sdroxide_types::Band, Vec<sdroxide_types::BandStackEntry>>;

fn load_json<T: serde::de::DeserializeOwned + Default>(file: &str) -> T {
    let Ok(dir) = config_dir() else { return T::default() };
    match fs::read_to_string(dir.join(file)) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            warn!("failed to parse {file}: {e}; starting fresh");
            T::default()
        }),
        Err(_) => T::default(),
    }
}

fn save_json<T: serde::Serialize>(file: &str, value: &T) -> Result<(), ConfigError> {
    let dir = config_dir()?;
    fs::create_dir_all(&dir)?;
    let text = serde_json::to_string_pretty(value).expect("serialize");
    fs::write(dir.join(file), text)?;
    Ok(())
}

pub fn load_bandstacks() -> BandStacks {
    load_json("bandstacks.json")
}

pub fn save_bandstacks(stacks: &BandStacks) -> Result<(), ConfigError> {
    save_json("bandstacks.json", stacks)
}

pub fn load_memories() -> Vec<sdroxide_types::MemoryChannel> {
    load_json("memories.json")
}

pub fn save_memories(memories: &[sdroxide_types::MemoryChannel]) -> Result<(), ConfigError> {
    save_json("memories.json", &memories)
}

/// Radio backend config (SoapySDR vs CAT rig; serial + sound-card settings).
pub fn load_radio_config() -> sdroxide_types::RadioConfig {
    load_json("radio.json")
}

pub fn save_radio_config(cfg: &sdroxide_types::RadioConfig) -> Result<(), ConfigError> {
    save_json("radio.json", cfg)
}

/// FT8/FT4 operator config (own call, grid, message templates).
pub fn load_digi_config() -> sdroxide_types::DigiConfig {
    load_json("digi.json")
}

pub fn save_digi_config(cfg: &sdroxide_types::DigiConfig) -> Result<(), ConfigError> {
    save_json("digi.json", cfg)
}

/// Skimmer preferences (per-kind enable + squelch). The operator's choice, kept
/// separate from the live `RadioState.skimmer`: a narrowband (audio-mode)
/// source forces the skimmers off, and that must not overwrite what the
/// operator picked for a wideband one.
pub fn load_skimmer_config() -> sdroxide_types::SkimmerSettings {
    load_json("skimmer.json")
}

pub fn save_skimmer_config(cfg: &sdroxide_types::SkimmerSettings) -> Result<(), ConfigError> {
    save_json("skimmer.json", cfg)
}

/// FSQ contacts (address book for directed FSQCALL messaging).
pub fn load_contacts() -> Vec<sdroxide_types::FsqContact> {
    load_json("contacts.json")
}

pub fn save_contacts(contacts: &[sdroxide_types::FsqContact]) -> Result<(), ConfigError> {
    save_json("contacts.json", &contacts)
}

/// Persistent logbook (digital + manual QSO entries).
pub fn load_qso_log() -> Vec<sdroxide_types::QsoRecord> {
    load_json("qso_log.json")
}

pub fn save_qso_log(log: &[sdroxide_types::QsoRecord]) -> Result<(), ConfigError> {
    save_json("qso_log.json", &log)
}

/// Network cockpit config (spot feeds, callsign lookup, uploads; credentials).
pub fn load_network_config() -> sdroxide_types::NetworkConfig {
    load_json("net.json")
}

pub fn save_network_config(cfg: &sdroxide_types::NetworkConfig) -> Result<(), ConfigError> {
    save_json("net.json", cfg)
}

/// Built-in TCI server config (the listener third-party TCI clients connect
/// to). Owned by the engine, like the network-cockpit config above.
pub fn load_tci_server_config() -> sdroxide_types::TciServerConfig {
    load_json("tciserver.json")
}

pub fn save_tci_server_config(cfg: &sdroxide_types::TciServerConfig) -> Result<(), ConfigError> {
    save_json("tciserver.json", cfg)
}

/// Built-in Hamlib rigctld server config (the listener "NET rigctl" clients
/// connect to). Owned by the engine, like the TCI server config above.
pub fn load_rigctld_config() -> sdroxide_types::RigctldConfig {
    load_json("rigctld.json")
}

pub fn save_rigctld_config(cfg: &sdroxide_types::RigctldConfig) -> Result<(), ConfigError> {
    save_json("rigctld.json", cfg)
}

/// WSJT-X UDP broadcast config (where decode/QSO datagrams are sent). Owned by
/// the engine, like the server configs above.
pub fn load_wsjtx_config() -> sdroxide_types::WsjtxConfig {
    load_json("wsjtx.json")
}

pub fn save_wsjtx_config(cfg: &sdroxide_types::WsjtxConfig) -> Result<(), ConfigError> {
    save_json("wsjtx.json", cfg)
}

/// Control-input bindings: keyboard chords, panadapter mouse behaviour and the
/// MIDI mapping. Unlike the configs above this one belongs to the *client*, not
/// the engine — it describes the hardware on the operator's desk, so a knob
/// keeps working when the UI drives a remote engine over `--connect`.
pub fn load_input_settings() -> sdroxide_types::InputSettings {
    load_json("input.json")
}

pub fn save_input_settings(cfg: &sdroxide_types::InputSettings) -> Result<(), ConfigError> {
    save_json("input.json", cfg)
}

/// SSTV per-slot transmit overlay messages (one entry per image slot). The
/// image pixels live as PNGs under [`sstv_tx_dir`]; this stores just the text
/// that is composited over each slot's picture.
pub fn load_sstv_messages() -> Vec<String> {
    load_json("sstv_messages.json")
}

pub fn save_sstv_messages(messages: &[String]) -> Result<(), ConfigError> {
    save_json("sstv_messages.json", &messages)
}

/// The operator's satellite additions: element sets pasted in by hand, and
/// frequency entries that override or extend the built-in table.
///
/// A client-side file like `input.json`: it describes what this operator wants
/// to track and where they have found the transponders, so it stays with the UI
/// rather than with the engine.
pub fn load_sat_config() -> sdroxide_types::SatConfig {
    let mut cfg: sdroxide_types::SatConfig = load_json("satellites.json");
    // The amateur satellites and the ISS used to be fetched unconditionally.
    // They are subscriptions now, so a config that predates them — or a fresh
    // install with no file at all — has to be given them once, or the sky comes
    // up empty. Written back immediately so the seeding happens exactly once
    // and unsubscribing sticks.
    if cfg.seed_defaults() {
        if let Err(e) = save_sat_config(&cfg) {
            warn!("could not write the seeded satellite subscriptions: {e}");
        }
    }
    cfg
}

pub fn save_sat_config(cfg: &sdroxide_types::SatConfig) -> Result<(), ConfigError> {
    save_json("satellites.json", cfg)
}

/// The broadcast station list (`broadcast_stations.json`).
pub const BROADCAST_STATIONS_FILE: &str = "broadcast_stations.json";

/// Where the broadcast station list lives, for showing in the settings panel.
pub fn broadcast_stations_path() -> Result<PathBuf, ConfigError> {
    Ok(config_dir()?.join(BROADCAST_STATIONS_FILE))
}

/// The longwave/shortwave broadcast stations the waterfall labels.
///
/// Unlike every other store here this file is *reference data*, not the
/// operator's preferences: it is seeded once from the table compiled into the
/// binary and then left alone, because someone who has corrected a schedule or
/// added a local station must not lose it to an upgrade. Deleting the file
/// restores the bundled list; [`restore_bundled_broadcast_stations`] does the
/// same deliberately.
///
/// A malformed file falls back to the bundled table rather than to an empty
/// list — the generic [`load_json`] behaviour of quietly starting fresh is
/// wrong for data the operator never expected to have to maintain.
pub fn load_broadcast_stations() -> Vec<sdroxide_types::BroadcastStation> {
    let Ok(path) = broadcast_stations_path() else {
        return sdroxide_types::broadcast::builtin().to_vec();
    };
    load_broadcast_stations_at(&path)
}

/// [`load_broadcast_stations`] against an explicit path, so the seed-once and
/// fall-back-on-garbage behaviour can be tested without a config directory.
fn load_broadcast_stations_at(path: &std::path::Path) -> Vec<sdroxide_types::BroadcastStation> {
    if !path.exists() {
        match write_bundled_broadcast_stations(path) {
            Ok(()) => info!("seeded {BROADCAST_STATIONS_FILE} from the bundled station list"),
            Err(e) => warn!("could not seed {BROADCAST_STATIONS_FILE}: {e}"),
        }
    }
    match fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<sdroxide_types::BroadcastStations>(&text) {
            Ok(f) => f.stations,
            Err(e) => {
                warn!("failed to parse {BROADCAST_STATIONS_FILE}: {e}; using the bundled list");
                sdroxide_types::broadcast::builtin().to_vec()
            }
        },
        Err(e) => {
            warn!("failed to read {BROADCAST_STATIONS_FILE}: {e}; using the bundled list");
            sdroxide_types::broadcast::builtin().to_vec()
        }
    }
}

pub fn save_broadcast_stations(
    stations: &[sdroxide_types::BroadcastStation],
) -> Result<(), ConfigError> {
    let file = sdroxide_types::BroadcastStations {
        version: 1,
        updated: String::new(),
        note: String::new(),
        stations: stations.to_vec(),
    };
    save_json(BROADCAST_STATIONS_FILE, &file)
}

/// Replace the operator's station list with the bundled one, keeping the old
/// file alongside as `.bak`. The only path that overwrites their edits, so it
/// stays behind an explicit action in the UI.
pub fn restore_bundled_broadcast_stations() -> Result<(), ConfigError> {
    restore_bundled_broadcast_stations_at(&broadcast_stations_path()?)
}

fn restore_bundled_broadcast_stations_at(path: &std::path::Path) -> Result<(), ConfigError> {
    if path.exists() {
        let backup = path.with_extension("json.bak");
        if let Err(e) = fs::rename(path, &backup) {
            warn!("could not back up {BROADCAST_STATIONS_FILE}: {e}");
        }
    }
    write_bundled_broadcast_stations(path)
}

/// Write the compiled-in table out verbatim, so the shipped and on-disk copies
/// are byte-identical — re-serialising would drop the file's own provenance
/// notes and reformat every entry.
fn write_bundled_broadcast_stations(path: &std::path::Path) -> Result<(), ConfigError> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, sdroxide_types::broadcast::EMBEDDED_JSON)?;
    Ok(())
}

/// Voice-keyer slot labels (one entry per slot). The recordings themselves are
/// WAV files under [`voice_dir`]; this stores only what each slot is called.
pub fn load_voice_names() -> Vec<String> {
    load_json("voice_names.json")
}

pub fn save_voice_names(names: &[String]) -> Result<(), ConfigError> {
    save_json("voice_names.json", &names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digi_config_roundtrip_via_json() {
        let cfg = sdroxide_types::DigiConfig {
            my_call: "AB1CD".into(),
            my_grid: "FN42".into(),
            ..Default::default()
        };
        let text = serde_json::to_string_pretty(&cfg).unwrap();
        let back: sdroxide_types::DigiConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(back.my_call, "AB1CD");
        assert_eq!(back.my_grid, "FN42");
        assert_eq!(back, cfg);
    }

    #[test]
    fn skimmer_config_roundtrip_via_json() {
        use sdroxide_types::{SkimmerKind, SkimmerSettings};
        let mut cfg = SkimmerSettings::default();
        cfg.set_enabled(SkimmerKind::Psk, false);
        cfg.set_squelch_db(SkimmerKind::Cw, 12);
        let back: SkimmerSettings =
            serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back, cfg);
        assert!(!back.enabled(SkimmerKind::Psk));
        assert_eq!(back.squelch_db(SkimmerKind::Cw), 12);
    }

    #[test]
    fn skimmer_config_fills_missing_fields() {
        // A file written before one of the fields existed still loads.
        let cfg: sdroxide_types::SkimmerSettings =
            serde_json::from_str(r#"{"enabled":[false,false,true]}"#).unwrap();
        assert_eq!(cfg.enabled, [false, false, true]);
        assert_eq!(cfg.squelch_db, sdroxide_types::SkimmerSettings::default().squelch_db);
    }

    #[test]
    fn bandstacks_roundtrip_via_json() {
        use sdroxide_types::{Band, BandStackEntry, Mode};
        let mut stacks = BandStacks::default();
        stacks.insert(
            Band::M40,
            vec![BandStackEntry {
                freq_hz: 7_100_000.0,
                mode: Mode::Lsb,
                filter_lo: -2850.0,
                filter_hi: -150.0,
            }],
        );
        let text = serde_json::to_string(&stacks).unwrap();
        let back: BandStacks = serde_json::from_str(&text).unwrap();
        assert_eq!(back, stacks);
    }

    #[test]
    fn default_settings_roundtrip_via_toml() {
        let s = Settings::default();
        let text = toml::to_string_pretty(&s).unwrap();
        let back: Settings = toml::from_str(&text).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn partial_file_fills_defaults() {
        let s: Settings = toml::from_str("sample_rate = 2400000.0").unwrap();
        assert_eq!(s.sample_rate, 2_400_000.0);
        assert_eq!(s.server_port, Settings::default().server_port);
    }

    #[test]
    fn network_config_loads_without_the_freedv_section() {
        // A net.json written before FreeDV Reporter existed.
        let c: sdroxide_types::NetworkConfig =
            serde_json::from_str(r#"{"spot_max_age_secs":600}"#).unwrap();
        assert_eq!(c.spot_max_age_secs, 600);
        assert_eq!(c.freedv_reporter, sdroxide_types::FreeDvReporterConfig::default());
    }

    #[test]
    fn network_config_ignores_the_retired_operator_identity_keys() {
        // net.json used to hold its own copy of the operator callsign and grid,
        // and the reporter section briefly held a third. All of that now comes
        // from the digi config, so a file still carrying them must load and
        // ignore them rather than fail.
        let c: sdroxide_types::NetworkConfig = serde_json::from_str(
            r#"{"my_call":"AB1CD","my_grid":"FN42","spot_max_age_secs":600,
                "cluster":{"enabled":true,"host":"cluster.example","port":7373},
                "freedv_reporter":{"enabled":true,"callsign":"OLD","grid":"AA00"}}"#,
        )
        .unwrap();
        assert_eq!(c.spot_max_age_secs, 600, "the rest of the file still applies");
        assert!(c.cluster.enabled);
        assert!(c.freedv_reporter.enabled);
    }

    /// A scratch directory of our own, so the station-list tests never touch the
    /// operator's real config. No `tempfile` dependency for four tests.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sdroxide-bc-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch dir");
        dir.join(BROADCAST_STATIONS_FILE)
    }

    #[test]
    fn a_missing_station_list_is_seeded_from_the_bundled_one() {
        let path = scratch("seed");
        assert!(!path.exists());
        let loaded = load_broadcast_stations_at(&path);
        assert!(path.exists(), "the file should have been written");
        // Byte-for-byte, so the shipped and seeded copies cannot drift and the
        // file's own provenance notes survive.
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            sdroxide_types::broadcast::EMBEDDED_JSON
        );
        assert_eq!(loaded.len(), sdroxide_types::broadcast::builtin().len());
    }

    #[test]
    fn an_edited_station_list_survives_being_loaded_again() {
        let path = scratch("edit");
        load_broadcast_stations_at(&path);
        fs::write(
            &path,
            r#"{"version":1,"stations":[{"name":"My Local Pirate","freq_khz":6295}]}"#,
        )
        .unwrap();
        let loaded = load_broadcast_stations_at(&path);
        assert_eq!(loaded.len(), 1, "the operator's file wins, and is not re-seeded");
        assert_eq!(loaded[0].name, "My Local Pirate");
        assert_eq!(loaded[0].freq_khz, 6295.0);
        // A two-field entry is legal, and everything else defaults.
        assert!(loaded[0].site.is_empty());
        assert_eq!(loaded[0].mode_str(), "AM");
    }

    #[test]
    fn a_corrupt_station_list_falls_back_without_overwriting_it() {
        let path = scratch("corrupt");
        fs::write(&path, "{ this is not json").unwrap();
        let loaded = load_broadcast_stations_at(&path);
        assert_eq!(
            loaded.len(),
            sdroxide_types::broadcast::builtin().len(),
            "a broken file must not leave the waterfall with no stations"
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{ this is not json",
            "and must not be silently destroyed — it is the operator's to fix"
        );
    }

    #[test]
    fn restoring_the_bundled_list_keeps_the_old_one_alongside() {
        let path = scratch("restore");
        fs::write(&path, r#"{"version":1,"stations":[{"name":"Mine","freq_khz":6070}]}"#).unwrap();
        restore_bundled_broadcast_stations_at(&path).expect("restore");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            sdroxide_types::broadcast::EMBEDDED_JSON
        );
        let backup = path.with_extension("json.bak");
        assert!(backup.exists(), "the edited list should be kept as .json.bak");
        assert!(fs::read_to_string(&backup).unwrap().contains("Mine"));
    }
}
