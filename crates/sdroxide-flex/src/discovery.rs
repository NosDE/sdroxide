//! FlexRadio discovery: radios broadcast themselves once a second to UDP port
//! 4992, so discovery is passive listening rather than a request/response scan
//! (unlike HPSDR's).
//!
//! The datagram is a VITA-49 extension packet with class code `0xFFFF` whose
//! payload is a plain `key=value` string — the same shape as a status line, so
//! it goes through the same field parser.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use sdroxide_types::FlexDevice;

use crate::protocol::{self, DISCOVERY_PORT};
use crate::vita;

/// Extract the payload string of a discovery datagram, or `None` if this isn't
/// one.
fn payload_str(buf: &[u8]) -> Option<String> {
    let pkt = vita::parse(buf)?;
    if pkt.class_code != vita::class::DISCOVERY {
        return None;
    }
    // Trailing NULs pad the payload out to a word boundary.
    let text = String::from_utf8_lossy(pkt.payload);
    Some(text.trim_end_matches(['\0', ' ']).to_string())
}

/// Build a device from a discovery payload. The `ip` field the radio sends is
/// authoritative (a radio on a second interface reports the address it wants to
/// be reached on), falling back to the datagram source.
fn parse_payload(payload: &str, src: Option<Ipv4Addr>) -> Option<FlexDevice> {
    let f = |k: &str| protocol::field(payload, k).unwrap_or("").to_string();
    let ip = match protocol::field(payload, "ip") {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => src?.to_string(),
    };
    let model = f("model");
    if model.is_empty() {
        return None;
    }
    // Names travel with spaces replaced by underscores.
    let name = {
        let n = if !f("nickname").is_empty() { f("nickname") } else { f("name") };
        n.replace('_', " ")
    };
    Some(FlexDevice {
        ip,
        model,
        serial: f("serial"),
        version: f("version"),
        name,
        callsign: f("callsign"),
        // `status=In_Use` (or a non-empty client list) means somebody already
        // has the radio; we can still connect as a second client.
        in_use: f("status").eq_ignore_ascii_case("in_use")
            || !protocol::field(payload, "gui_client_ips").unwrap_or("").is_empty(),
    })
}

/// Bind the discovery port so that a SmartSDR or DAX instance already listening
/// on this machine keeps working. Both flags are needed: Linux wants
/// `SO_REUSEADDR` for a second binder of a broadcast port, the BSDs (macOS)
/// want `SO_REUSEPORT`.
fn bind_shared(port: u16) -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    #[cfg(not(target_os = "windows"))]
    sock.set_reuse_port(true)?;
    sock.set_broadcast(true)?;
    sock.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)).into())?;
    Ok(sock.into())
}

/// Listen for radios announcing themselves, collecting for `timeout`. Radios
/// broadcast once a second, so anything under ~2 s risks missing one that is
/// present.
pub fn discover(timeout: Duration) -> Vec<FlexDevice> {
    let socket = match bind_shared(DISCOVERY_PORT) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("Flex discovery: bind {DISCOVERY_PORT} failed: {e}");
            return Vec::new();
        }
    };
    let _ = socket.set_read_timeout(Some(Duration::from_millis(200)));

    let deadline = Instant::now() + timeout;
    let mut found: Vec<FlexDevice> = Vec::new();
    let mut buf = [0u8; 2048];
    while Instant::now() < deadline {
        match socket.recv_from(&mut buf) {
            Ok((n, src)) => {
                let src_v4 = match src {
                    SocketAddr::V4(a) => Some(*a.ip()),
                    SocketAddr::V6(_) => None,
                };
                let Some(payload) = payload_str(&buf[..n]) else {
                    tracing::trace!("Flex discovery: ignored {n}-byte datagram from {src}");
                    continue;
                };
                tracing::trace!("Flex discovery: {src} → {payload}");
                if let Some(dev) = parse_payload(&payload, src_v4) {
                    if let Some(existing) = found.iter_mut().find(|d| d.ip == dev.ip) {
                        *existing = dev; // keep the freshest announcement
                    } else {
                        found.push(dev);
                    }
                }
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => {
                tracing::debug!("Flex discovery recv error: {e}");
                break;
            }
        }
    }
    found.sort_by(|a, b| a.ip.cmp(&b.ip));
    tracing::info!(
        "Flex discovery: {} radio(s) found{}",
        found.len(),
        if found.is_empty() {
            String::new()
        } else {
            format!(
                ": {}",
                found.iter().map(|d| d.label()).collect::<Vec<_>>().join(", ")
            )
        }
    );
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wrap a payload string the way a radio does, so the parser is exercised
    /// through the real framing.
    fn discovery_datagram(payload: &str) -> Vec<u8> {
        let mut body = payload.as_bytes().to_vec();
        while !body.len().is_multiple_of(4) {
            body.push(0);
        }
        let mut pkt = Vec::new();
        pkt.push((vita::packet_type::EXT_DATA_WITH_STREAM << 4) | 0x08);
        pkt.push(0x00);
        let words = ((vita::HEADER_LEN + body.len()) / 4) as u16;
        pkt.extend_from_slice(&words.to_be_bytes());
        pkt.extend_from_slice(&0x0000_0800u32.to_be_bytes()); // discovery stream id
        pkt.extend_from_slice(&vita::FLEX_OUI.to_be_bytes());
        pkt.extend_from_slice(&0x534C_FFFFu32.to_be_bytes());
        pkt.extend_from_slice(&0u32.to_be_bytes());
        pkt.extend_from_slice(&0u64.to_be_bytes());
        pkt.extend_from_slice(&body);
        pkt
    }

    #[test]
    fn parses_a_broadcast() {
        let payload = "discovery_protocol_version=3.0.0.2 model=FLEX-8600 \
                       serial=1234-5678-9012-3456 version=4.0.7.42 nickname=Shack_Radio \
                       callsign=DL1ABC ip=192.168.1.50 port=4992 status=Available";
        let dev =
            parse_payload(&payload_str(&discovery_datagram(payload)).expect("payload"), None)
                .expect("device");
        assert_eq!(dev.ip, "192.168.1.50");
        assert_eq!(dev.model, "FLEX-8600");
        assert_eq!(dev.name, "Shack Radio");
        assert_eq!(dev.version, "4.0.7.42");
        assert_eq!(dev.callsign, "DL1ABC");
        assert!(!dev.in_use);
    }

    #[test]
    fn radios_with_a_gui_client_are_flagged_in_use() {
        let payload = "model=FLEX-6600 serial=42 ip=10.0.0.5 status=In_Use \
                       gui_client_ips=10.0.0.9";
        let dev = parse_payload(payload, None).expect("device");
        assert!(dev.in_use);
    }

    #[test]
    fn falls_back_to_the_source_address() {
        let dev = parse_payload("model=FLEX-6400 serial=7", Some(Ipv4Addr::new(10, 0, 0, 7)))
            .expect("device");
        assert_eq!(dev.ip, "10.0.0.7");
    }

    #[test]
    fn other_vita_traffic_is_not_discovery() {
        // A DAX audio packet on the same socket must not read as a radio.
        let pkt = vita::encode_dax_audio(0x2000_0001, 0, &[0.0, 0.0]);
        assert!(payload_str(&pkt).is_none());
        // Nor may a discovery packet without a model become a device.
        assert!(parse_payload("serial=42 ip=10.0.0.5", None).is_none());
    }
}
