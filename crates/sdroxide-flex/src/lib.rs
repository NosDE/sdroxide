//! Native FlexRadio (SmartSDR) client for the FLEX-6000 and FLEX-8000 series.
//!
//! NATIVE ONLY. Plain TCP/UDP sockets; this crate must never be a dependency of
//! any wasm-targeted crate.
//!
//! sdroxide connects as a **GUI client**, so it drives the radio on its own
//! with no SmartSDR running: it creates a panadapter, a slice (the radio's VFO),
//! a DAX IQ stream for receive and a DAX TX audio stream for transmit, and
//! removes all of them again when the connection ends.
//!
//! Receive is wideband IQ (sdroxide demodulates); transmit is audio the radio
//! modulates — the same division of labour as the TCI backend.
//!
//! * [`protocol`] — the line protocol on TCP 4992 and the mode mapping.
//! * [`vita`] — VITA-49 framing of the UDP streams.
//! * [`meters`] — meter ids, units and scaling.
//! * [`discover`] — radios announcing themselves on the LAN.
//! * [`FlexHandle`] — a live connection.

mod net;

pub mod discovery;
pub mod meters;
pub mod protocol;
pub mod vita;

use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

pub use discovery::discover;
pub use net::{FlexError, FlexHandle, FlexUpdate, TX_RATE_HZ};

/// How long a discovery scan listens by default. Radios announce themselves
/// once a second, so this catches two announcements from each.
pub const DISCOVERY_WINDOW: Duration = Duration::from_millis(2500);

/// Listen for radios with the default window.
pub fn discover_default() -> Vec<sdroxide_types::FlexDevice> {
    discovery::discover(DISCOVERY_WINDOW)
}

/// Test a radio: connect to its command port, read the greeting, and report the
/// model and SmartSDR version — or why it didn't work. Used by the Settings
/// dialog's "Test connection" button, so it must never block for long.
pub fn test_connection(ip: &str, timeout: Duration) -> Result<String, String> {
    let addr: Ipv4Addr = ip.trim().parse().map_err(|_| format!("invalid IP address {ip:?}"))?;
    let sockaddr = SocketAddr::from((addr, protocol::CONTROL_PORT));
    let stream = TcpStream::connect_timeout(&sockaddr, timeout.min(Duration::from_secs(3)))
        .map_err(|e| format!("connect {sockaddr}: {e}"))?;
    stream.set_read_timeout(Some(Duration::from_millis(200))).ok();

    let mut conn = LineReader { stream, partial: Vec::new() };
    // Ask for the radio object as well, so the answer names the rig rather than
    // just proving something is listening.
    let _ = conn.write(&format!("C1|{}\n", protocol::sub("radio all")));

    let deadline = Instant::now() + timeout;
    let mut version = String::new();
    let mut model = String::new();
    let mut callsign = String::new();
    loop {
        if Instant::now() > deadline {
            return if version.is_empty() {
                Err("no answer from the radio (is SmartSDR's API port reachable?)".into())
            } else {
                // The greeting arrived, the radio object didn't — still a
                // working connection.
                Ok(format!("FlexRadio, SmartSDR {version}"))
            };
        }
        for line in conn.read_lines().map_err(|e| format!("read: {e}"))? {
            match protocol::parse_line(&line) {
                Some(protocol::Line::Version(v)) => version = v,
                Some(protocol::Line::Status { body, .. }) if protocol::object(&body) == "radio" => {
                    if let Some(m) = protocol::field(&body, "model") {
                        model = m.replace('_', " ");
                    }
                    if let Some(c) = protocol::field(&body, "callsign") {
                        callsign = c.to_string();
                    }
                    if !model.is_empty() {
                        let mut s = model.clone();
                        if !version.is_empty() {
                            s = format!("{s}  [SmartSDR {version}]");
                        }
                        if !callsign.is_empty() {
                            s = format!("{s}, {callsign}");
                        }
                        return Ok(s);
                    }
                }
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Minimal line reader for [`test_connection`] — the streaming path has its own
/// inside [`net`].
struct LineReader {
    stream: TcpStream,
    partial: Vec<u8>,
}

impl LineReader {
    fn write(&mut self, s: &str) -> std::io::Result<()> {
        use std::io::Write;
        self.stream.write_all(s.as_bytes())
    }

    fn read_lines(&mut self) -> std::io::Result<Vec<String>> {
        use std::io::Read;
        let mut buf = [0u8; 4096];
        match self.stream.read(&mut buf) {
            Ok(0) => return Err(std::io::Error::other("radio closed the connection")),
            Ok(n) => self.partial.extend_from_slice(&buf[..n]),
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(e),
        }
        let mut out = Vec::new();
        while let Some(nl) = self.partial.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = self.partial.drain(..=nl).collect();
            out.push(String::from_utf8_lossy(&raw).trim_end().to_string());
        }
        Ok(out)
    }
}
