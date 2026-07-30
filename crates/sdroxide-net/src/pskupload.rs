//! PSK Reporter reception-report *upload*: tell the network what this station
//! is hearing, so it appears as a receiver on <https://pskreporter.info>.
//!
//! The collector speaks IPFIX (RFC 5101) over UDP with PSK Reporter's own
//! vendor fields (enterprise number 30351). A packet is: the 16-byte IPFIX
//! header, the two template definitions, one record describing *us* (the
//! receiver), and one record per station heard.
//!
//! The wire format is built by [`encode_packet`], which is pure and unit-tested
//! byte for byte — the collector gives no feedback at all (fire-and-forget UDP,
//! reports simply never appear if the framing is wrong), so the tests are the
//! only place the format is checked.
//!
//! Reports are batched: PSK Reporter asks for no more than one packet every
//! five minutes per station, and only the best report per callsign in that
//! window is worth sending.

use std::net::{ToSocketAddrs, UdpSocket};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use sdroxide_types::PskConfig;

use crate::event::{EventTx, NetEvent};

/// PSK Reporter's IANA private enterprise number.
const ENTERPRISE: u32 = 30351;
/// Our template IDs. Any value ≥ 256 works; these match what other clients use.
const TEMPLATE_SENDER: u16 = 0x5001;
const TEMPLATE_RECEIVER: u16 = 0x5002;
/// Keep packets inside the smallest MTU we might cross.
const MAX_PACKET: usize = 1400;
/// Never batch more than this many reports, however long the interval.
const MAX_PENDING: usize = 512;
/// The shortest gap the collector accepts between packets from one station.
const MIN_INTERVAL_S: u64 = 300;

/// One station we heard.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub call: String,
    pub grid: String,
    /// The signal's own frequency (dial + audio offset), in Hz.
    pub freq_hz: u32,
    pub snr_db: i8,
    /// "FT8", "FT4", …
    pub mode: String,
    /// Unix seconds of the slot it was decoded from.
    pub when_utc: u32,
}

/// Who is doing the hearing — the record that makes us a station on the map.
#[derive(Debug, Clone, PartialEq)]
pub struct Station {
    pub call: String,
    pub grid: String,
    pub software: String,
    pub antenna: String,
}

// ── Wire format ─────────────────────────────────────────────────────────────

fn push_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn push_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

/// An IPFIX variable-length string: one length byte, then the bytes. Anything
/// longer than the collector's fields is truncated rather than dropped.
fn push_str(out: &mut Vec<u8>, s: &str) {
    let b = s.as_bytes();
    let n = b.len().min(60);
    out.push(n as u8);
    out.extend_from_slice(&b[..n]);
}

/// One template field specifier: vendor fields set the top bit of the field id
/// and carry the enterprise number; `len` is `0xFFFF` for variable length.
fn push_field(out: &mut Vec<u8>, id: u16, len: u16, vendor: bool) {
    push_u16(out, if vendor { id | 0x8000 } else { id });
    push_u16(out, len);
    if vendor {
        push_u32(out, ENTERPRISE);
    }
}

/// Pad a set out to a 4-byte boundary, as IPFIX requires.
fn pad4(out: &mut Vec<u8>, set_start: usize) {
    while (out.len() - set_start) % 4 != 0 {
        out.push(0);
    }
}

/// The template describing a reception report (what we heard).
fn sender_template(out: &mut Vec<u8>) {
    let start = out.len();
    push_u16(out, 2); // set id 2 = template set
    push_u16(out, 0); // length, patched below
    push_u16(out, TEMPLATE_SENDER);
    push_u16(out, 7); // field count
    push_field(out, 1, 0xFFFF, true); // senderCallsign
    push_field(out, 5, 4, true); // frequency
    push_field(out, 6, 1, true); // sNR
    push_field(out, 10, 0xFFFF, true); // mode
    push_field(out, 3, 0xFFFF, true); // senderLocator
    push_field(out, 11, 1, true); // informationSource
    push_field(out, 150, 4, false); // flowStartSeconds (IANA)
    pad4(out, start);
    let len = (out.len() - start) as u16;
    out[start + 2..start + 4].copy_from_slice(&len.to_be_bytes());
}

/// The template describing the receiving station (us).
fn receiver_template(out: &mut Vec<u8>) {
    let start = out.len();
    push_u16(out, 3); // set id 3 = options template set
    push_u16(out, 0); // length, patched below
    push_u16(out, TEMPLATE_RECEIVER);
    push_u16(out, 4); // field count
    push_u16(out, 0); // scope field count
    push_field(out, 2, 0xFFFF, true); // receiverCallsign
    push_field(out, 4, 0xFFFF, true); // receiverLocator
    push_field(out, 8, 0xFFFF, true); // decodingSoftware
    push_field(out, 9, 0xFFFF, true); // antennaInformation
    pad4(out, start);
    let len = (out.len() - start) as u16;
    out[start + 2..start + 4].copy_from_slice(&len.to_be_bytes());
}

/// Build one datagram: header, both templates, our receiver record, and as many
/// reports as fit. Returns the packet and how many reports it took.
///
/// `seq` is the number of data records sent before this packet and `domain` a
/// per-session identifier; both are IPFIX bookkeeping the collector uses to
/// spot lost datagrams.
pub fn encode_packet(
    rx: &Station,
    reports: &[Report],
    now_utc: u32,
    seq: u32,
    domain: u32,
) -> (Vec<u8>, usize) {
    let mut out = Vec::with_capacity(MAX_PACKET);
    // Header (patched with the final length once the packet is built).
    push_u16(&mut out, 0x000A); // IPFIX version
    push_u16(&mut out, 0); // length
    push_u32(&mut out, now_utc);
    push_u32(&mut out, seq);
    push_u32(&mut out, domain);

    // Both templates travel in every packet: they cost ~100 bytes and mean a
    // datagram is never orphaned by an earlier one going missing.
    sender_template(&mut out);
    receiver_template(&mut out);

    // Receiver record.
    let start = out.len();
    push_u16(&mut out, TEMPLATE_RECEIVER);
    push_u16(&mut out, 0);
    push_str(&mut out, &rx.call);
    push_str(&mut out, &rx.grid);
    push_str(&mut out, &rx.software);
    push_str(&mut out, &rx.antenna);
    pad4(&mut out, start);
    let len = (out.len() - start) as u16;
    out[start + 2..start + 4].copy_from_slice(&len.to_be_bytes());

    // Reception reports, up to the packet budget.
    let start = out.len();
    push_u16(&mut out, TEMPLATE_SENDER);
    push_u16(&mut out, 0);
    let mut used = 0;
    for r in reports {
        let before = out.len();
        push_str(&mut out, &r.call);
        push_u32(&mut out, r.freq_hz);
        out.push(r.snr_db as u8);
        push_str(&mut out, &r.mode);
        push_str(&mut out, &r.grid);
        out.push(1); // informationSource: 1 = automatically decoded
        push_u32(&mut out, r.when_utc);
        if out.len() + 3 > MAX_PACKET {
            out.truncate(before); // doesn't fit — leave it for the next packet
            break;
        }
        used += 1;
    }
    pad4(&mut out, start);
    let len = (out.len() - start) as u16;
    out[start + 2..start + 4].copy_from_slice(&len.to_be_bytes());

    let total = out.len() as u16;
    out[2..4].copy_from_slice(&total.to_be_bytes());
    (out, used)
}

// ── The upload worker ───────────────────────────────────────────────────────

/// A running uploader. Dropping it flushes what's pending and stops the thread.
pub struct PskUploadHandle {
    tx: Sender<Report>,
    stop: Sender<()>,
    join: Option<JoinHandle<()>>,
}

impl Drop for PskUploadHandle {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl PskUploadHandle {
    /// Queue a station we heard. Cheap and non-blocking: the batch goes out on
    /// the worker's own schedule.
    pub fn report(&self, r: Report) {
        let _ = self.tx.send(r);
    }
}

/// Start the uploader for `rx`. Reports are batched and sent every
/// `interval_secs` (floored at the collector's five-minute minimum).
pub fn spawn(
    cfg: &PskConfig,
    rx: Station,
    events: EventTx,
    now_utc: fn() -> i64,
) -> PskUploadHandle {
    let (tx, report_rx) = crossbeam_channel::unbounded::<Report>();
    let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(1);
    let host = if cfg.host.trim().is_empty() {
        "report.pskreporter.info".to_string()
    } else {
        cfg.host.trim().to_string()
    };
    let port = if cfg.port == 0 { 4739 } else { cfg.port };
    let interval = Duration::from_secs((cfg.interval_secs as u64).max(MIN_INTERVAL_S));
    let join = std::thread::Builder::new()
        .name("sdroxide-pskupload".into())
        .spawn(move || run(host, port, rx, interval, report_rx, stop_rx, events, now_utc))
        .expect("spawn psk upload worker");
    PskUploadHandle { tx, stop: stop_tx, join: Some(join) }
}

#[allow(clippy::too_many_arguments)]
fn run(
    host: String,
    port: u16,
    rx: Station,
    interval: Duration,
    reports: Receiver<Report>,
    stop: Receiver<()>,
    events: EventTx,
    now_utc: fn() -> i64,
) {
    // A per-session observation domain, from the callsign and the start time —
    // constant for this run, different from the last one.
    let domain = {
        let mut h: u32 = 2_166_136_261;
        for b in rx.call.bytes().chain((now_utc() as u32).to_be_bytes()) {
            h = (h ^ b as u32).wrapping_mul(16_777_619);
        }
        h | 1
    };
    let mut pending: Vec<Report> = Vec::new();
    let mut seq: u32 = 0;
    let mut last_send = Instant::now();
    let mut reported_error = false;
    loop {
        let done = match reports.recv_timeout(Duration::from_millis(500)) {
            Ok(r) => {
                merge(&mut pending, r);
                false
            }
            Err(RecvTimeoutError::Timeout) => stop.try_recv().is_ok(),
            Err(RecvTimeoutError::Disconnected) => true,
        };
        let due = last_send.elapsed() >= interval;
        if (due || done) && !pending.is_empty() {
            let batch = std::mem::take(&mut pending);
            match send_batch(&host, port, &rx, &batch, now_utc() as u32, &mut seq, domain) {
                Ok(n) => {
                    reported_error = false;
                    let _ = events.send(NetEvent::Status(Some(format!(
                        "PSK Reporter: {n} report{} uploaded",
                        if n == 1 { "" } else { "s" }
                    ))));
                }
                // One status line per outage, not one per interval.
                Err(e) if !reported_error => {
                    reported_error = true;
                    let _ = events.send(NetEvent::Status(Some(format!("PSK Reporter: {e}"))));
                }
                Err(_) => {}
            }
            last_send = Instant::now();
        }
        if done {
            break;
        }
    }
}

/// Fold a report into the pending batch: one entry per callsign per band, the
/// strongest signal winning, since that is the report the network keeps anyway.
fn merge(pending: &mut Vec<Report>, r: Report) {
    if let Some(e) =
        pending.iter_mut().find(|e| e.call == r.call && e.freq_hz.abs_diff(r.freq_hz) < 10_000)
    {
        if r.snr_db >= e.snr_db {
            *e = r;
        }
        return;
    }
    if pending.len() < MAX_PENDING {
        pending.push(r);
    }
}

/// Send `batch` as one or more datagrams. Returns how many reports went out.
fn send_batch(
    host: &str,
    port: u16,
    rx: &Station,
    batch: &[Report],
    now: u32,
    seq: &mut u32,
    domain: u32,
) -> Result<usize, String> {
    let addr = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("{host}:{port}: {e}"))?
        .next()
        .ok_or_else(|| format!("{host}:{port}: no address"))?;
    let sock = UdpSocket::bind(if addr.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" })
        .map_err(|e| e.to_string())?;
    let mut sent = 0;
    while sent < batch.len() {
        let (pkt, used) = encode_packet(rx, &batch[sent..], now, *seq, domain);
        if used == 0 {
            break; // a single report that can't fit is not worth a retry loop
        }
        sock.send_to(&pkt, addr).map_err(|e| e.to_string())?;
        *seq += used as u32 + 1; // reports plus the receiver record
        sent += used;
    }
    Ok(sent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rx() -> Station {
        Station {
            call: "AB1CD".into(),
            grid: "FN42".into(),
            software: "sdroxide".into(),
            antenna: String::new(),
        }
    }

    fn report(call: &str, snr: i8) -> Report {
        Report {
            call: call.into(),
            grid: "EM48".into(),
            freq_hz: 14_075_500,
            snr_db: snr,
            mode: "FT8".into(),
            when_utc: 1_700_000_000,
        }
    }

    fn be16(b: &[u8], at: usize) -> u16 {
        u16::from_be_bytes([b[at], b[at + 1]])
    }

    #[test]
    fn packet_layout_matches_ipfix() {
        let (pkt, used) = encode_packet(&rx(), &[report("W9XYZ", -12)], 1_700_000_100, 0, 0x1234);
        assert_eq!(used, 1);
        assert_eq!(be16(&pkt, 0), 0x000A, "IPFIX version");
        assert_eq!(be16(&pkt, 2) as usize, pkt.len(), "header length is the real length");
        assert_eq!(u32::from_be_bytes(pkt[4..8].try_into().unwrap()), 1_700_000_100);
        assert_eq!(u32::from_be_bytes(pkt[12..16].try_into().unwrap()), 0x1234);

        // Walk the sets: two templates, then the receiver record, then reports.
        let mut at = 16;
        let mut ids = Vec::new();
        while at < pkt.len() {
            let (id, len) = (be16(&pkt, at), be16(&pkt, at + 2) as usize);
            assert!(len >= 4 && at + len <= pkt.len(), "set {id:#x} length {len} at {at}");
            assert_eq!(len % 4, 0, "set {id:#x} is not padded to a 4-byte boundary");
            ids.push(id);
            at += len;
        }
        assert_eq!(at, pkt.len(), "sets exactly fill the packet");
        assert_eq!(ids, vec![2, 3, TEMPLATE_RECEIVER, TEMPLATE_SENDER]);

        // The strings are length-prefixed, and ours is in the receiver record.
        let call = pkt.windows(6).any(|w| w == b"\x05AB1CD");
        assert!(call, "receiver callsign is not length-prefixed in the packet");
        assert!(pkt.windows(6).any(|w| w == b"\x05W9XYZ"), "sender callsign missing");
    }

    #[test]
    fn a_full_batch_splits_across_packets() {
        // Every report has to go somewhere: no packet may exceed the MTU
        // budget, and the batch loop must make progress on each pass.
        let reports: Vec<Report> = (0..200).map(|i| report(&format!("W9XY{i:03}"), -10)).collect();
        let mut sent = 0;
        let mut packets = 0;
        while sent < reports.len() {
            let (pkt, used) = encode_packet(&rx(), &reports[sent..], 0, sent as u32, 1);
            assert!(pkt.len() <= MAX_PACKET, "packet of {} bytes", pkt.len());
            assert!(used > 0, "no progress at report {sent}");
            sent += used;
            packets += 1;
        }
        assert!(packets > 1, "200 reports should not fit in one datagram");
    }

    #[test]
    fn the_batch_keeps_the_best_report_per_station() {
        let mut pending = Vec::new();
        merge(&mut pending, report("W9XYZ", -15));
        merge(&mut pending, report("W9XYZ", -8)); // same station, stronger
        merge(&mut pending, report("K1ABC", -20));
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].snr_db, -8);

        // A weaker later report doesn't replace the better one.
        merge(&mut pending, report("W9XYZ", -19));
        assert_eq!(pending[0].snr_db, -8);

        // The same callsign on another band is its own report.
        let mut other = report("W9XYZ", -11);
        other.freq_hz = 7_074_000;
        merge(&mut pending, other);
        assert_eq!(pending.len(), 3);
    }
}
