//! WebSocket wire protocol between the sdroxide server and remote clients.
//!
//! Framing: every binary WS message is `[PROTO_VERSION_BYTE][postcard bytes]`.
//! The version byte is a fast sanity check; the real version negotiation
//! happens in `Hello`/`HelloAck`.
//!
//! Compiles for native and `wasm32-unknown-unknown`.

pub mod solar;

use serde::{Deserialize, Serialize};

use sdroxide_types::{
    CallsignInfo, Command, Decode, DeviceCaps, DigiStatus, MemoryChannel, Meters, QsoRecord,
    RadioState, RifpMeta, RifpStatus, SkimmerSpot, SpectrumFrame, Spot, SstvMode, SstvStatus,
    UploadResult, VoiceStatus,
};

/// Bump on any incompatible change to the message enums (this includes the
/// payload structs from `sdroxide-types` that ride the wire, e.g. `QsoRecord`).
/// v3: `QsoRecord` gained `id` + `comment` fields.
/// v4: added `ServerMsg::SkimmerSpots` + `Command::SetSkimmerEnabled` + a
/// `RadioState.skimmer_enabled` field.
/// v5: added SSTV — `Mode::Sstv`, `ServerMsg::Sstv*`, and
/// `Command::SstvTx`/`SstvSetMode`.
/// v6: added audio noise reduction + auto-notch — `Command::SetNoiseReduction`,
/// `Command::SetAutoNotch`, and `RxState.noise_reduction` / `RxState.auto_notch`.
/// v7: added keyboard modes Olivia/Thor/FSQ — new `Mode` variants, `DigiConfig`
/// submode fields (Olivia tones/bw, THOR submode, FSQ speed/call), `DigiStatus`
/// FSQ heard-list + directed-message fields, and a mode-agnostic digi image path
/// (`Command::DigiImageTx` / `RadioEvent::DigiImage` for the FSQ image sub-mode).
/// v8: added the audio recorder — `Command::SetRecording` and
/// `RadioState.recording` / `RadioState.recording_file`.
/// v9: FT8/FT4 QSO handling — `QsoStep::WaitCq` / `Confirming` and
/// `Command::DigiStartQso.wait_for_cq`.
/// v10: neural (RNNoise) noise reduction — new `NrLevel::Ai{Low,Med,High}`
/// variants can appear in `RxState.noise_reduction`.
/// v11: network cockpit — spot feeds, callsign lookup, and uploads. New
/// `Command::SetNetworkConfig`/`SpotDialHint`/`LookupCallsign`/`UploadQso`/
/// `SyncConfirmations` and `ServerMsg::Spots`/`NetStatus`/`CallsignResult`/
/// `Upload`/`Confirmations`, plus new `QsoRecord` fields.
/// v12: per-kind skimmer control — `RadioState.skimmer_enabled` became
/// `RadioState.skimmer: SkimmerSettings` (CW/PSK/RTTY enables + squelch) and
/// `Command::SetSkimmerEnabled` became `Command::SetSkimmerConfig`.
/// v13: built-in TCI server — `Command::SetTciServerConfig` and
/// `ServerMsg::TciServerStatus`.
/// v14: FreeDV Reporter — new `SpotKind::FreeDv` (extends the postcard
/// discriminant space of `ServerMsg::Spots`) and `NetworkConfig`'s new
/// `freedv_reporter` field. `NetworkConfig` also lost `my_call`/`my_grid`: the
/// operator identity is `DigiConfig`'s alone, so both ends must agree on the
/// shape `Command::SetNetworkConfig` carries.
/// v15: built-in Hamlib rigctld server — `Command::SetRigctldConfig` and
/// `ServerMsg::RigctldStatus` (both extend the postcard discriminant space).
/// v16: FT8 message handling and reporting. `Decode` gained `cq_dx` and
/// `free_text`, `TranscriptLine` gained `overheard`, and `PskConfig` gained the
/// upload fields — postcard is not self-describing, so every added field
/// changes the layout of the messages carrying them. Also new:
/// `Command::SetWsjtxConfig` (WSJT-X UDP broadcast).
/// v17: manual FT8 message control — `Command::DigiSetStep` and
/// `Command::DigiSendText` (both extend the postcard discriminant space).
/// v18: FT8 transmit watchdog — `DigiStatus.tx_watchdog` plus `DigiConfig`'s
/// `tx_watchdog_min` / `max_tx_repeats`, which both ends must agree on.
/// v19: voice keyer — `Command::VoiceRecord`/`VoicePlay`/`VoicePreview`/
/// `VoiceClear`/`VoiceRename` and `ServerMsg::VoiceStatus` (both extend the
/// postcard discriminant space).
/// v20: Hellschreiber — `ServerMsg::HellColumns` plus `DigiConfig`'s
/// `hell_variant` / `hell_rx_agc`, which both ends must agree on because
/// `DigiStatus` carries the config. (`Mode::Hell` alone would have been
/// compatible: it is appended to the enum, so no existing discriminant moves.)
/// v21: FT8 DXpedition mode — `DigiConfig`'s `dxped_mode` / `fox_slots`,
/// `DigiStatus.fox_queue`, and `Decode.rr73_to` (the RR73 half of a Fox
/// message, which is how a Hound learns its contact completed). Postcard is not
/// self-describing, so both ends must agree on every one of those fields.
/// v22: clock-offset monitoring — `DigiStatus.clock_offset_s`.
/// v23: directed CQs — `Decode.cq_dx` became `Decode.cq_to`, the modifier
/// itself (`DX`, `EU`, `JA`, `POTA`, …) rather than a single DX flag.
/// v24: the FT8/FT4 call queue — `Command::DigiQueueAdd`/`DigiQueueRemove` and
/// `DigiStatus.call_queue`.
/// v25: automatic transmit-frequency choice — `DigiConfig.auto_tx_freq`.
/// v26: RIFP (draft-dulaunoy-rifp-00) — `Mode::Rifp` and `Band::M70` (both
/// appended, so no existing discriminant moves), `Command::RifpTx` /
/// `RifpDropSession`, `ServerMsg::RifpRows` / `RifpImage` / `RifpStatus`, and
/// `DigiConfig`'s `rifp_*` fields, which both ends must agree on because
/// `DigiStatus` carries the config.
/// v27: WFM broadcast stereo — `RxState.wfm_stereo`, `Meters.stereo` and
/// `Command::SetWfmStereo`. The command is appended so no existing discriminant
/// moves, but postcard is not self-describing, so the two added struct fields
/// change the layout of every message carrying `RadioState` or `Meters`.
/// v28: JS8 — `Mode::Js8`, the `js8_*` fields on `DigiConfig`, and
/// `DigiStatus.js8` carrying the heard list, the reassembled conversation and
/// transmit-queue progress. No message enum gained a variant, but postcard is
/// not self-describing and the added struct fields change the layout of every
/// message carrying `DigiConfig` or `DigiStatus`.
/// v29: JS8 beaconing — `DigiConfig`'s `js8_hb_ack` (answer a heard heartbeat
/// with a signal report) and `js8_hb_anywhere` (beacon on the working frequency
/// instead of the 500–1000 Hz sub-band), plus `Js8Status.hb_hz`, the frequency
/// the last beacon actually went out on. `Js8Status.next_hb_in_s` is now
/// populated rather than always `None`, which is a behaviour change but not a
/// layout one. Both ends must agree on the three added fields, postcard being
/// what it is.
/// v30: broadcast station labels — new `SpotKind::Broadcast`, which extends the
/// postcard discriminant space of `ServerMsg::Spots` exactly as `FreeDv` did in
/// v14. The engine never emits it (the stations are synthesised client-side from
/// a bundled table), but the enum both ends decode has changed shape, so they
/// must agree on it.
pub const PROTO_VERSION: u16 = 30;
const VERSION_BYTE: u8 = 0x12;

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("empty message")]
    Empty,
    #[error("unsupported protocol version byte {0:#x}")]
    Version(u8),
    #[error("decode error: {0}")]
    Decode(#[from] postcard::Error),
}

/// Audio codec for one stream direction, negotiated at Hello time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioCodec {
    /// 20 ms Opus frames, 48 kHz mono.
    Opus48kMono,
    /// Little-endian PCM16, 48 kHz mono (fallback when WebCodecs is missing).
    Pcm16_48k,
}

/// What the client can encode/decode (browser WebCodecs availability).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioCaps {
    pub opus_decode: bool,
    pub opus_encode: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClientMsg {
    Hello {
        proto: u16,
        audio: AudioCaps,
    },
    Command(Command),
    /// 20 ms mic frame in the codec negotiated at Hello.
    MicFrame {
        seq: u32,
        payload: Vec<u8>,
    },
    Ping(u64),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ServerMsg {
    HelloAck {
        proto: u16,
        caps: DeviceCaps,
        state: RadioState,
        /// Codec of server→client RX audio.
        rx_codec: AudioCodec,
        /// Codec expected for client→server mic frames.
        tx_codec: AudioCodec,
    },
    State(RadioState),
    Spectrum(SpectrumFrame),
    Meters(Meters),
    Memories(Vec<MemoryChannel>),
    RxAudio {
        seq: u32,
        payload: Vec<u8>,
    },
    Pong(u64),
    /// Another client already holds the (single) session.
    Busy,
    Error(String),
    // FT8/FT4 digital modes.
    Ft8Decodes(Vec<Decode>),
    Ft8Status(DigiStatus),
    Ft8QsoLogged(QsoRecord),
    // Skimmers (CW etc.).
    SkimmerSpots(Vec<SkimmerSpot>),
    // SSTV image mode.
    SstvLine {
        image_id: u32,
        y: u16,
        rgb: Vec<u8>,
    },
    SstvImage {
        image_id: u32,
        mode: SstvMode,
        w: u16,
        h: u16,
        png: Vec<u8>,
    },
    SstvStatus(SstvStatus),
    // Weather fax (receive only).
    WefaxLine {
        image_id: u32,
        y: u16,
        gray: Vec<u8>,
    },
    WefaxImage {
        image_id: u32,
        w: u16,
        h: u16,
        png: Vec<u8>,
    },
    WefaxStatus(sdroxide_types::WefaxStatus),
    // RIFP image mode.
    /// Reassembled raster rows of an incoming picture (grayscale, `w` per row).
    RifpRows {
        image_id: u32,
        y: u16,
        w: u16,
        h: u16,
        rows: Vec<u8>,
    },
    /// A completed, digest-verified picture (PNG bytes) and its manifest facts.
    RifpImage {
        image_id: u32,
        meta: RifpMeta,
        png: Vec<u8>,
    },
    RifpStatus(RifpStatus),
    /// FSQ image: a completed received picture (PNG bytes).
    DigiImage {
        png: Vec<u8>,
    },
    /// Hellschreiber: a batch of received dot columns, column-major, 0 = black.
    /// `seq` is the absolute column index so a client can detect a dropped
    /// batch — this lane drops rather than blocks when it backs up, and Hell has
    /// no framing of its own to resynchronise against.
    HellColumns {
        seq: u64,
        rows: u8,
        cols: Vec<u8>,
    },
    /// Voice keyer: slot contents plus what is being recorded or transmitted.
    VoiceStatus(VoiceStatus),
    // Network cockpit.
    Spots(Vec<Spot>),
    NetStatus(Option<String>),
    CallsignResult(CallsignInfo),
    Upload(UploadResult),
    Confirmations(Vec<QsoRecord>),
    /// Built-in TCI server status (listener up, bind address, client count).
    TciServerStatus {
        running: bool,
        addr: String,
        clients: usize,
        error: Option<String>,
    },
    /// Built-in rigctld server status, so the settings dialog on a remote
    /// client can show what the engine's listener is doing.
    RigctldStatus {
        running: bool,
        addr: String,
        clients: usize,
        error: Option<String>,
    },
}

pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, ProtoError> {
    Ok(postcard::to_extend(msg, vec![VERSION_BYTE])?)
}

pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, ProtoError> {
    match bytes {
        [] => Err(ProtoError::Empty),
        [VERSION_BYTE, rest @ ..] => Ok(postcard::from_bytes(rest)?),
        [v, ..] => Err(ProtoError::Version(*v)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_client_and_server_msgs() {
        let msgs = [
            ClientMsg::Hello {
                proto: PROTO_VERSION,
                audio: AudioCaps { opus_decode: true, opus_encode: false },
            },
            ClientMsg::Command(Command::SetPtt(true)),
            ClientMsg::MicFrame { seq: 7, payload: vec![1, 2, 3] },
        ];
        for m in &msgs {
            let bytes = encode(m).unwrap();
            let back: ClientMsg = decode(&bytes).unwrap();
            assert_eq!(&back, m);
        }

        let m = ServerMsg::State(RadioState::default());
        let bytes = encode(&m).unwrap();
        let back: ServerMsg = decode(&bytes).unwrap();
        assert_eq!(back, m);

        // SSTV image/status messages round-trip (binary pixel payloads).
        let sstv = [
            ServerMsg::SstvLine { image_id: 3, y: 7, rgb: vec![1, 2, 3, 4, 5, 6] },
            ServerMsg::SstvImage {
                image_id: 3,
                mode: SstvMode::Martin1,
                w: 320,
                h: 256,
                png: vec![0x89, 0x50, 0x4e, 0x47],
            },
            ServerMsg::SstvStatus(SstvStatus {
                tx_mode: SstvMode::Robot36,
                detected: Some(SstvMode::Scottie2),
                ..SstvStatus::default()
            }),
        ];
        for m in &sstv {
            let bytes = encode(m).unwrap();
            let back: ServerMsg = decode(&bytes).unwrap();
            assert_eq!(&back, m);
        }

        // RIFP carries pixels, a manifest summary, and a per-chunk map.
        let rifp = [
            ServerMsg::RifpRows { image_id: 2, y: 11, w: 4, h: 20, rows: vec![9, 8, 7, 6] },
            ServerMsg::RifpImage {
                image_id: 2,
                meta: RifpMeta {
                    session: "0123456789abcdef".into(),
                    filename: "oe1test.png".into(),
                    sender: Some("OE1TEST".into()),
                    hint: None,
                    media_type: "image/png".into(),
                    content_encoding: "identity".into(),
                    width: 320,
                    height: 240,
                    bits_per_pixel: 4,
                    encoded_size: 9_000,
                    chunk_count: 47,
                    chunks_first_pass: 45,
                },
                png: vec![0x89, 0x50, 0x4e, 0x47],
            },
            ServerMsg::RifpStatus(RifpStatus {
                tx_active: true,
                tx_progress: 0.25,
                sessions: vec![sdroxide_types::RifpSession {
                    session: "0123456789abcdef".into(),
                    sender: None,
                    have_manifest: true,
                    have: 3,
                    total: 47,
                    map: vec![0b0000_0111],
                    idle_s: 2,
                }],
                ..RifpStatus::default()
            }),
        ];
        for m in &rifp {
            let bytes = encode(m).unwrap();
            let back: ServerMsg = decode(&bytes).unwrap();
            assert_eq!(&back, m);
        }
    }

    #[test]
    fn rejects_wrong_version_byte() {
        assert!(matches!(decode::<ClientMsg>(&[0x7f, 0, 0]), Err(ProtoError::Version(0x7f))));
        assert!(matches!(decode::<ClientMsg>(&[]), Err(ProtoError::Empty)));
    }
}
