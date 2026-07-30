//! The SmartSDR line protocol on TCP port 4992: command builders, the four
//! kinds of line a radio sends back, and the Rust `Mode` ↔ SmartSDR mode map.
//!
//! Everything here is plain text, one message per line (`\n`). A client sends
//! `C<seq>|<command>`; the radio answers `R<seq>|<hex code>|<body>`, pushes
//! object changes as `S<handle>|<object> <fields…>`, and reports events as
//! `M<hex num>|<text>`. The first two lines after connecting are the API
//! version (`V…`) and the handle this connection was given (`H…`).
//!
//! Documented in the FlexRadio `smartsdr-api-docs` wiki; the FLEX-8000 speaks
//! the same command set as the 6000 series.

use sdroxide_types::{AtuState, Mode};

/// TCP port carrying the command/status protocol.
pub const CONTROL_PORT: u16 = 4992;
/// UDP port the radio listens on for the streams we send it (TX audio).
pub const VITA_PORT: u16 = 4991;
/// UDP port radios broadcast their discovery packets to.
pub const DISCOVERY_PORT: u16 = 4992;

/// A line the radio sent us.
#[derive(Debug, Clone, PartialEq)]
pub enum Line {
    /// `V<version>` — the API version, sent first on connect.
    Version(String),
    /// `H<hex>` — the handle identifying this connection. Status messages
    /// caused by our own commands carry it.
    Handle(u32),
    /// `R<seq>|<hex code>|<body>` — the answer to command `seq`. `code` is zero
    /// on success; the body carries whatever the command returns (a stream id,
    /// a client id, …).
    Response { seq: u32, code: u32, body: String },
    /// `S<handle>|<body>` — an object (slice, interlock, meter, …) changed.
    Status { handle: u32, body: String },
    /// `M<hex num>|<text>` — a log/alarm message from the radio.
    Message { code: u32, text: String },
}

fn hex(s: &str) -> Option<u32> {
    u32::from_str_radix(s.trim().trim_start_matches("0x").trim_start_matches("0X"), 16).ok()
}

/// Parse one line from the radio. Returns `None` for empty lines and for
/// anything whose prefix we don't know (forward compatibility: SmartSDR gains
/// message kinds between versions, and an unknown one must not be fatal).
pub fn parse_line(line: &str) -> Option<Line> {
    let line = line.trim_end_matches(['\r', '\n']);
    let (tag, rest) = line.split_at_checked(1)?;
    match tag {
        "V" => Some(Line::Version(rest.trim().to_string())),
        "H" => hex(rest).map(Line::Handle),
        "R" => {
            let (seq, rest) = rest.split_once('|')?;
            // The body may itself contain '|' (debug output is appended after
            // it), so split only the status code off the front.
            let (code, body) = rest.split_once('|').unwrap_or((rest, ""));
            Some(Line::Response {
                seq: seq.trim().parse().ok()?,
                code: hex(code)?,
                body: body.to_string(),
            })
        }
        "S" => {
            let (handle, body) = rest.split_once('|')?;
            Some(Line::Status { handle: hex(handle)?, body: body.to_string() })
        }
        "M" => {
            let (num, text) = rest.split_once('|')?;
            Some(Line::Message { code: hex(num).unwrap_or(0), text: text.to_string() })
        }
        _ => None,
    }
}

/// Iterate the `key=value` tokens of a status body, skipping the leading object
/// words (`slice 0`, `interlock`, …) and any token without a `=`.
pub fn fields(body: &str) -> impl Iterator<Item = (&str, &str)> {
    body.split_whitespace().filter_map(|tok| tok.split_once('='))
}

/// Value of one `key=value` token in a status body. Keys are matched
/// case-insensitively — SmartSDR mixes spellings (`RF_frequency`, `in_use`).
pub fn field<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    fields(body).find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v)
}

/// The object words of a status body, i.e. everything before the first
/// `key=value` token: `"slice 0"` for `slice 0 in_use=1 …`.
pub fn object(body: &str) -> &str {
    let mut at = 0;
    let mut end = 0;
    for tok in body.split(' ') {
        if tok.contains('=') {
            break;
        }
        at += tok.len();
        end = at;
        at += 1; // the separating space
    }
    body[..end].trim()
}

/// Parse a `0x…` (or bare hex) object/stream id.
pub fn parse_id(s: &str) -> Option<u32> {
    hex(s)
}

/// Format a frequency for the wire: SmartSDR speaks MHz with 6 decimals (1 Hz
/// resolution).
pub fn mhz(hz: f64) -> String {
    format!("{:.6}", hz / 1e6)
}

// ── Commands ──
// Builders return the command body only; the connection prepends `C<seq>|` and
// appends the newline, so the sequence number stays owned by one place.

/// Register as a GUI client — what a stand-alone client (no SmartSDR running)
/// must be to own slices and a panadapter. Passing the `client_id` a previous
/// session was given keeps the same station identity on the radio.
pub fn client_gui(client_id: Option<&str>) -> String {
    match client_id {
        Some(id) if !id.trim().is_empty() => format!("client gui {}", id.trim()),
        _ => "client gui".to_string(),
    }
}

/// Announce the program name (informational, shown in SmartSDR's client list).
pub fn client_program(name: &str) -> String {
    format!("client program {}", name.replace(' ', "_"))
}

/// Announce the station name shown next to the client on the radio.
pub fn client_station(name: &str) -> String {
    format!("client station {}", name.replace(' ', "_"))
}

/// Tell the radio which UDP port to send our VITA-49 streams to. Must be sent
/// once, before any stream is created.
pub fn client_udpport(port: u16) -> String {
    format!("client udpport {port}")
}

/// Subscribe to an object class's status stream (`sub slice all`, `sub tx all`,
/// `sub meter all`, …).
pub fn sub(what: &str) -> String {
    format!("sub {what}")
}

/// Create a panadapter. The DAX IQ channel takes its centre frequency and
/// bandwidth from the panadapter it is bound to, so we need one even though we
/// never display the radio's own FFT. Responds with `<pan id>,<waterfall id>`.
pub fn pan_create(freq_hz: f64, x: u32, y: u32) -> String {
    format!("display pan c freq={} x={x} y={y}", mhz(freq_hz))
}

/// Set panadapter parameters (`center=`, `bandwidth=`, `fps=`, …), values in
/// MHz where they are frequencies.
pub fn pan_set(pan: u32, params: &str) -> String {
    format!("display pan s 0x{pan:08X} {params}")
}

/// Centre the panadapter — and with it the DAX IQ stream — on `hz`.
pub fn pan_center(pan: u32, hz: f64) -> String {
    pan_set(pan, &format!("center={}", mhz(hz)))
}

/// Set the panadapter's displayed bandwidth. The DAX IQ rate is independent of
/// this, but keeping them equal makes the radio's own display match what
/// sdroxide receives.
pub fn pan_bandwidth(pan: u32, hz: f64) -> String {
    pan_set(pan, &format!("bandwidth={}", mhz(hz)))
}

/// Ask the radio which RF-gain settings a panadapter accepts. The answer is
/// the radio's own list — the FLEX-6000 and 8000 families differ, and a
/// transverter changes it again, so it is asked for rather than assumed.
pub fn pan_rfgain_info(pan: u32) -> String {
    format!("display pan rfgain_info 0x{pan:08X}")
}

/// Set the panadapter's RF gain (preamp/attenuator ahead of the converter).
/// This is the one gain that changes the DAX IQ samples we receive — the
/// radio's own AGC sits in the slice, downstream of our tap.
pub fn pan_rfgain(pan: u32, db: f64) -> String {
    pan_set(pan, &format!("rfgain={}", db.round() as i32))
}

/// Parse an `rfgain_info` reply into the settings it offers, sorted. The reply
/// is a comma-separated list of decibel values; anything unparsable is skipped
/// so an unexpected shape costs the gain control, not the connection.
pub fn parse_rfgain_info(body: &str) -> Vec<f64> {
    let mut out: Vec<f64> =
        body.split([',', ' ']).filter_map(|t| t.trim().parse::<f64>().ok()).collect();
    out.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    out.dedup();
    out
}

pub fn pan_remove(pan: u32) -> String {
    format!("display pan r 0x{pan:08X}")
}

/// Create a slice receiver on a panadapter. The slice is the radio's own VFO:
/// it carries the frequency and mode the operator sees, and it is what
/// transmits.
pub fn slice_create(freq_hz: f64, pan: u32, mode: Mode, ant: &str) -> String {
    let mut s =
        format!("slice create freq={} pan=0x{pan:08X} mode={}", mhz(freq_hz), mode_to_flex(mode));
    if !ant.trim().is_empty() {
        s.push_str(&format!(" ant={}", ant.trim()));
    }
    s
}

/// Tune a slice to `hz`.
pub fn slice_tune(slice: u32, hz: f64) -> String {
    format!("slice t {slice} {}", mhz(hz))
}

/// Set slice parameters (`mode=`, `tx=`, `active=`, `rxant=`, `txant=`, …).
pub fn slice_set(slice: u32, params: &str) -> String {
    format!("slice s {slice} {params}")
}

pub fn slice_mode(slice: u32, mode: Mode) -> String {
    slice_set(slice, &format!("mode={}", mode_to_flex(mode)))
}

pub fn slice_remove(slice: u32) -> String {
    format!("slice r {slice}")
}

pub fn slice_list() -> String {
    "slice list".to_string()
}

/// Create the DAX IQ stream for `channel`. Responds with the stream id the
/// VITA-49 packets will carry.
pub fn stream_create_dax_iq(channel: u32) -> String {
    format!("stream create type=dax_iq daxiq_channel={channel}")
}

/// Create the DAX TX audio stream — the path our modulating audio takes into
/// the radio. Responds with the stream id we must stamp our packets with.
pub fn stream_create_dax_tx() -> String {
    "stream create type=dax_tx".to_string()
}

pub fn stream_remove(stream_id: u32) -> String {
    format!("stream remove 0x{stream_id:08X}")
}

/// Bind a DAX IQ channel to a panadapter and set its sample rate (24, 48, 96 or
/// 192 kHz — see [`IQ_RATES`]).
///
/// This is the spelling in the published command list. On v3 and later radios
/// the binding that actually routes samples is the *panadapter's*
/// [`pan_daxiq_channel`] plus the stream's [`stream_daxiq_rate`]; this one is
/// sent as well because it is what the documentation asks for, and a radio that
/// ignores it simply answers with an error.
pub fn dax_iq_set(channel: u32, pan: u32, rate_hz: f64) -> String {
    format!("dax iq set {channel} pan=0x{pan:08X} rate={}", (rate_hz / 1000.0).round() as u32)
}

/// Point a panadapter at a DAX IQ channel. Without this the radio has nothing
/// feeding the channel and the stream stays silent, however well the `stream
/// create` went.
pub fn pan_daxiq_channel(pan: u32, channel: u32) -> String {
    pan_set(pan, &format!("daxiq_channel={channel}"))
}

/// Set a DAX IQ stream's sample rate. The rate lives on the stream object and
/// is given in Hz (24000 / 48000 / 96000 / 192000) — a stream created without
/// it runs at 48 kHz whatever the panadapter says.
pub fn stream_daxiq_rate(stream_id: u32, rate_hz: f64) -> String {
    format!("stream set 0x{stream_id:08X} daxiq_rate={}", rate_hz.round() as u32)
}

/// Enable/disable the DAX TX audio path. Without this the radio ignores the
/// audio packets we send even while keyed.
pub fn dax_tx(on: bool) -> String {
    format!("dax tx {}", u8::from(on))
}

/// Key/unkey the transmitter (drives the radio's interlock state machine).
pub fn xmit(on: bool) -> String {
    format!("xmit {}", u8::from(on))
}

/// Transmit power, 0..100 %.
pub fn transmit_rfpower(percent: u32) -> String {
    format!("transmit set rfpower={}", percent.min(100))
}

/// TUNE power, 0..100 %.
pub fn transmit_tunepower(percent: u32) -> String {
    format!("transmit set tunepower={}", percent.min(100))
}

/// Plain-language text for the response codes a client actually runs into.
/// The radio answers with a bare hex number, which on its own tells an operator
/// nothing; SmartSDR's own error list (`SL_ERROR_BASE` + offset) is what turns
/// it into something actionable. Unlisted codes keep their hex.
pub fn code_text(code: u32) -> Option<&'static str> {
    Some(match code {
        0x5000_0001 => "no foundation receiver",
        0x5000_0002 => "the radio's license has no slice available",
        0x5000_0003 => "all slices are in use",
        0x5000_0004 => "bad slice parameter",
        0x5000_0009 => "no panadapter (foundation receiver) available",
        0x5000_000C => "frequency out of range",
        0x5000_0011 => "frequency too high",
        0x5000_0013 => "bad command",
        0x5000_0015 => "unknown command",
        0x5000_0016 => "malformed command",
        0x5000_002A => "unknown antenna port",
        0x5000_002C => "wrong number of parameters",
        0x5000_002D => "bad field",
        0x5000_0032 => "bad mode",
        0x5000_003D => "this radio cannot transmit",
        0x5000_0042 => "not ready to transmit",
        0x5000_0043 => "no transmitter",
        0x5000_0048 => "invalid RF power",
        0x5000_0058 => "PTT timeout",
        0x5000_0059 => "invalid stream id",
        0x5000_0062 => "invalid client",
        0x5000_0063 => "invalid frequency",
        0x5000_0064 => "no IP or port (the UDP port was never registered)",
        0x5000_0065 => "invalid DAX channel",
        0x5000_0066 => "invalid DAX IQ channel",
        0x5000_0067 => "invalid DAX IQ rate",
        0x5000_0068 => "the slice is locked",
        0x5000_0069 => "frequency too low",
        0x5000_006B => "DAX IQ is not available in full duplex",
        0x5000_0092 => "another client disconnected us",
        _ => return None,
    })
}

// ── Antenna tuner ──

/// Run a tune cycle on the built-in ATU. The radio transmits by itself for the
/// duration and reports progress as `atu status=…`.
pub fn atu_start() -> &'static str {
    "atu start"
}

/// Take the ATU out of circuit.
pub fn atu_bypass() -> &'static str {
    "atu bypass"
}

/// Forget the tuner's stored matches.
pub fn atu_clear() -> &'static str {
    "atu clear"
}

/// Rust [`AtuState`] for the radio's `atu status=` value.
///
/// `TUNE_OK` and `TUNE_SUCCESSFUL` both mean a match is in circuit; the three
/// unhappy endings (`TUNE_FAIL`, `TUNE_FAIL_BYPASS`, `TUNE_ABORTED`) all come
/// down to the same thing for the operator — run it again.
pub fn atu_state(status: &str) -> Option<AtuState> {
    Some(match status.trim().to_ascii_uppercase().as_str() {
        "NONE" | "TUNE_NOT_STARTED" => AtuState::NotStarted,
        "TUNE_IN_PROGRESS" => AtuState::Tuning,
        "TUNE_SUCCESSFUL" | "TUNE_OK" => AtuState::Success,
        "TUNE_BYPASS" => AtuState::Bypass,
        "TUNE_MANUAL_BYPASS" => AtuState::ManualBypass,
        "TUNE_FAIL" | "TUNE_FAIL_BYPASS" | "TUNE_ABORTED" => AtuState::Failed,
        _ => return None,
    })
}

/// The DAX IQ sample rates SmartSDR offers, in Hz.
pub const IQ_RATES: [f64; 4] = [24_000.0, 48_000.0, 96_000.0, 192_000.0];

/// DAX audio streams — including the TX audio we send — run at 24 kHz.
pub const DAX_AUDIO_RATE_HZ: u32 = 24_000;

// ── Modes ──

/// SmartSDR mode string for a Rust `Mode`. The data modes ride DIGU/DIGL the
/// way they do on a hardware rig's DATA setting: sdroxide's own modulator
/// produces the audio, the radio just needs a flat SSB passband.
pub fn mode_to_flex(mode: Mode) -> &'static str {
    match mode {
        Mode::Lsb => "LSB",
        // Hell and WEFAX are rasters on plain SSB, like SSTV.
        Mode::Usb | Mode::Sstv | Mode::Hell | Mode::Wefax | Mode::RfPaint | Mode::Spec => "USB",
        Mode::Cw => "CW",
        Mode::Am => "AM",
        Mode::Sam => "SAM",
        // RIFP is FSK straight on the carrier over a ~25 kHz channel — the
        // narrow FM passband is the one that fits it, as it is over CI-V.
        Mode::Nfm | Mode::Rifp => "NFM",
        Mode::Wfm => "FM",
        Mode::Dsb => "DSB",
        Mode::Digl => "DIGL",
        Mode::Digu
        | Mode::Ft8
        | Mode::Ft4
        | Mode::Psk
        | Mode::Rtty
        | Mode::Olivia
        | Mode::Thor
        | Mode::Fsq
        | Mode::Js8
        | Mode::Rade => "DIGU",
    }
}

/// Rust `Mode` for a SmartSDR mode string, so the dial and mode follow when the
/// operator changes them on the radio or in SmartSDR.
pub fn flex_to_mode(s: &str) -> Option<Mode> {
    Some(match s.trim().to_ascii_uppercase().as_str() {
        "LSB" => Mode::Lsb,
        "USB" => Mode::Usb,
        "CW" | "CWL" | "CWU" => Mode::Cw,
        "AM" => Mode::Am,
        "SAM" => Mode::Sam,
        "NFM" => Mode::Nfm,
        "FM" | "DFM" => Mode::Wfm,
        "DSB" => Mode::Dsb,
        "DIGU" | "FDV" => Mode::Digu,
        // The radio's own FSK RTTY has no sdroxide equivalent (`Mode::Rtty` is
        // our soft decoder over SSB audio, and mapping to it would command the
        // radio straight back out of RTTY). Its passband is the DIGL one, so
        // that is what we follow it with.
        "DIGL" | "RTTY" => Mode::Digl,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_connect_burst() {
        assert_eq!(parse_line("V1.4.0.0\n"), Some(Line::Version("1.4.0.0".into())));
        assert_eq!(parse_line("H2B3F1A9"), Some(Line::Handle(0x2B3F1A9)));
    }

    #[test]
    fn parses_responses() {
        // Success with a payload: the stream id a `stream create` returned.
        assert_eq!(
            parse_line("R12|0|0x20000001"),
            Some(Line::Response { seq: 12, code: 0, body: "0x20000001".into() })
        );
        // A failure code is hex, not decimal.
        let Some(Line::Response { code, .. }) = parse_line("R7|50000015|") else {
            panic!("not a response");
        };
        assert_eq!(code, 0x5000_0015);
        // Debug output adds further '|' sections; they stay in the body.
        assert_eq!(
            parse_line("R3|0|0x40000000,0x42000000|debug"),
            Some(Line::Response { seq: 3, code: 0, body: "0x40000000,0x42000000|debug".into() })
        );
    }

    #[test]
    fn parses_status_and_messages() {
        let l = parse_line("S2B3F1A9|slice 0 in_use=1 RF_frequency=14.074000 mode=DIGU");
        let Some(Line::Status { handle, body }) = l else { panic!("not a status") };
        assert_eq!(handle, 0x2B3F1A9);
        assert_eq!(object(&body), "slice 0");
        assert_eq!(field(&body, "rf_frequency"), Some("14.074000"));
        assert_eq!(field(&body, "MODE"), Some("DIGU"));
        assert_eq!(field(&body, "absent"), None);

        assert_eq!(
            parse_line("M10000001|Client connected"),
            Some(Line::Message { code: 0x1000_0001, text: "Client connected".into() })
        );
    }

    #[test]
    fn unknown_lines_are_ignored_not_fatal() {
        assert_eq!(parse_line(""), None);
        assert_eq!(parse_line("Z1|whatever"), None);
        assert_eq!(parse_line("S|no handle"), None);
    }

    #[test]
    fn object_words_stop_at_the_first_pair() {
        assert_eq!(object("interlock state=TRANSMITTING reason= source=SW"), "interlock");
        assert_eq!(object("meter 1.nam=FWDPWR"), "meter");
        assert_eq!(object("display pan 0x40000000 center=14.100"), "display pan 0x40000000");
    }

    #[test]
    fn commands() {
        assert_eq!(client_gui(None), "client gui");
        assert_eq!(client_gui(Some("72E8-C7F3")), "client gui 72E8-C7F3");
        assert_eq!(client_program("sdroxide"), "client program sdroxide");
        assert_eq!(client_station("Shack 1"), "client station Shack_1");
        assert_eq!(client_udpport(4993), "client udpport 4993");
        assert_eq!(sub("slice all"), "sub slice all");
        assert_eq!(
            pan_create(14_100_000.0, 1200, 300),
            "display pan c freq=14.100000 x=1200 y=300"
        );
        assert_eq!(
            pan_center(0x4000_0000, 14_074_000.0),
            "display pan s 0x40000000 center=14.074000"
        );
        assert_eq!(
            pan_bandwidth(0x4000_0000, 192_000.0),
            "display pan s 0x40000000 bandwidth=0.192000"
        );
        assert_eq!(
            slice_create(14_074_000.0, 0x4000_0000, Mode::Ft8, "ANT1"),
            "slice create freq=14.074000 pan=0x40000000 mode=DIGU ant=ANT1"
        );
        assert_eq!(slice_tune(0, 7_074_000.0), "slice t 0 7.074000");
        assert_eq!(slice_mode(1, Mode::Cw), "slice s 1 mode=CW");
        assert_eq!(stream_create_dax_iq(1), "stream create type=dax_iq daxiq_channel=1");
        assert_eq!(stream_create_dax_tx(), "stream create type=dax_tx");
        assert_eq!(stream_remove(0x2000_0001), "stream remove 0x20000001");
        assert_eq!(dax_iq_set(1, 0x4000_0000, 192_000.0), "dax iq set 1 pan=0x40000000 rate=192");
        assert_eq!(dax_tx(true), "dax tx 1");
        assert_eq!(xmit(false), "xmit 0");
        assert_eq!(transmit_rfpower(250), "transmit set rfpower=100"); // clamped
        assert_eq!(transmit_tunepower(10), "transmit set tunepower=10");
    }

    #[test]
    fn rfgain_info() {
        assert_eq!(pan_rfgain_info(0x4000_0000), "display pan rfgain_info 0x40000000");
        assert_eq!(pan_rfgain(0x4000_0000, 20.0), "display pan s 0x40000000 rfgain=20");
        assert_eq!(pan_rfgain(0x4000_0000, -8.4), "display pan s 0x40000000 rfgain=-8");
        // The radio's own list, in whatever order and spacing it sends it.
        assert_eq!(parse_rfgain_info("-10,0,10,20,30"), vec![-10.0, 0.0, 10.0, 20.0, 30.0]);
        assert_eq!(parse_rfgain_info("20, 10, 0"), vec![0.0, 10.0, 20.0]);
        // A shape we did not anticipate must not produce nonsense gains.
        assert!(parse_rfgain_info("").is_empty());
        assert!(parse_rfgain_info("low high step").is_empty());
    }

    #[test]
    fn atu_status_values() {
        assert_eq!(atu_start(), "atu start");
        assert_eq!(atu_bypass(), "atu bypass");
        // Both spellings of a match found.
        assert_eq!(atu_state("TUNE_SUCCESSFUL"), Some(AtuState::Success));
        assert_eq!(atu_state("TUNE_OK"), Some(AtuState::Success));
        // Every unhappy ending reads the same to the operator: try again.
        for s in ["TUNE_FAIL", "TUNE_FAIL_BYPASS", "TUNE_ABORTED"] {
            assert_eq!(atu_state(s), Some(AtuState::Failed), "{s}");
        }
        assert_eq!(atu_state("TUNE_BYPASS"), Some(AtuState::Bypass));
        assert_eq!(atu_state("TUNE_MANUAL_BYPASS"), Some(AtuState::ManualBypass));
        assert_eq!(atu_state("TUNE_IN_PROGRESS"), Some(AtuState::Tuning));
        assert_eq!(atu_state("NONE"), Some(AtuState::NotStarted));
        // A value from a future firmware must not be mistaken for one we know.
        assert_eq!(atu_state("TUNE_SOMETHING_NEW"), None);
        // Both bypass kinds read as "Bypass"; only a found match engages the
        // button.
        assert_eq!(AtuState::ManualBypass.label(), "Bypass");
        assert!(AtuState::Success.is_engaged());
        assert!(!AtuState::Bypass.is_engaged());
    }

    #[test]
    fn mode_round_trip() {
        for m in [Mode::Lsb, Mode::Usb, Mode::Cw, Mode::Am, Mode::Sam, Mode::Nfm, Mode::Digu] {
            assert_eq!(flex_to_mode(mode_to_flex(m)), Some(m), "{m:?}");
        }
        // Digital sub-modes map onto DIGU and come back as plain DIGU — the
        // digi engine, not the radio, knows which one is running.
        assert_eq!(mode_to_flex(Mode::Ft8), "DIGU");
        assert_eq!(flex_to_mode("digu"), Some(Mode::Digu));
        assert_eq!(flex_to_mode("nonsense"), None);
    }
}
