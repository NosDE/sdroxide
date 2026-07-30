//! Native TCI (Transceiver Control Interface) over WebSocket — both halves.
//!
//! NATIVE ONLY. Pure-Rust WebSocket (tungstenite); this crate must never be a
//! dependency of any wasm-targeted crate.
//!
//! [`TciHandle`] is the **client**: sdroxide as the operator of somebody else's
//! rig (ExpertSDR3, Thetis). Receive is wideband IQ (sdroxide demodulates);
//! transmit is audio (the rig modulates the audio we send). It is reached only
//! from the root binary and `local_controller.rs`; the settings UI talks to it
//! exclusively through the `RadioController` trait.
//!
//! [`server`] is the **server**: sdroxide as the rig, driven by third-party TCI
//! clients (WSJT-X, JTDX, MSHV, skimmers). It is owned by the DSP engine, which
//! feeds it IQ and RX audio and drains its client requests.
//!
//! [`protocol`] holds the wire format both halves share.

mod net;
pub mod protocol;
pub mod server;

use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use tungstenite::{HandshakeError, Message, WebSocket};

pub use net::{TX_RATE_HZ, TciError, TciHandle, TciUpdate};

/// Default TCI port (ExpertSDR3).
pub const DEFAULT_PORT: u16 = 50001;

/// How long the WebSocket upgrade itself may take, before any status is read.
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Read timeout held during the upgrade. Short enough to keep the deadline
/// below responsive, long enough that the usual case completes in one read.
pub(crate) const HANDSHAKE_POLL: Duration = Duration::from_millis(500);

/// Perform the WebSocket upgrade on an already-connected socket, retrying until
/// `deadline`.
///
/// Both callers put a read timeout on the socket (they poll the connection
/// afterwards), and tungstenite reports a read that timed out mid-upgrade as
/// `Interrupted` — the server simply hasn't answered yet, which is not a
/// failure. Resuming the handshake instead of giving up on the first
/// `Interrupted` is what makes connecting to a busy ExpertSDR3 reliable; taking
/// it as fatal is why a freshly started sdroxide so often needed a manual
/// "Apply / reconnect" to attach to a rig that was running all along.
pub(crate) fn ws_handshake(
    stream: TcpStream,
    url: &str,
    deadline: Instant,
) -> Result<WebSocket<TcpStream>, String> {
    let mut attempt = tungstenite::client(url, stream);
    loop {
        match attempt {
            Ok((ws, _response)) => return Ok(ws),
            Err(HandshakeError::Interrupted(mid)) => {
                if Instant::now() > deadline {
                    return Err("timed out during the WebSocket handshake".into());
                }
                attempt = mid.handshake();
            }
            Err(e) => return Err(format!("WebSocket handshake failed: {e}")),
        }
    }
}

/// Split a `host[:port]` (or `ws://host:port`) address into `(host, port)`,
/// defaulting the port to [`DEFAULT_PORT`].
pub(crate) fn split_addr(address: &str) -> Result<(String, u16), String> {
    let a = address.trim();
    let a = a.strip_prefix("ws://").or_else(|| a.strip_prefix("wss://")).unwrap_or(a);
    let a = a.trim_end_matches('/');
    match a.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => {
            let port: u16 = port.parse().map_err(|_| format!("invalid port in {address:?}"))?;
            Ok((host.to_string(), port))
        }
        _ => Ok((a.to_string(), DEFAULT_PORT)),
    }
}

/// Test a TCI server: connect, read the status burst until `ready;` (or
/// `timeout`), and return a one-line summary (device / protocol / IQ rates) or
/// an error message.
pub fn test_connection(address: &str, timeout: Duration) -> Result<String, String> {
    let (host, port) = split_addr(address)?;
    let sockaddr = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| format!("resolve {host}:{port}: {e}"))?
        .next()
        .ok_or_else(|| format!("no address for {host}:{port}"))?;
    let stream = TcpStream::connect_timeout(&sockaddr, timeout.min(Duration::from_secs(3)))
        .map_err(|e| format!("connect {host}:{port}: {e}"))?;
    stream.set_read_timeout(Some(HANDSHAKE_POLL)).ok();

    let url = format!("ws://{host}:{port}/");
    let deadline = Instant::now() + timeout;
    let upgrade_by = deadline.min(Instant::now() + HANDSHAKE_TIMEOUT);
    let mut ws = ws_handshake(stream, url.as_str(), upgrade_by)?;
    // Status polling from here on: don't sit in a blocking read.
    ws.get_ref().set_read_timeout(Some(Duration::from_millis(250))).ok();

    let mut device = String::new();
    let mut protocol = String::new();
    let mut iq_rate = String::new();
    loop {
        if Instant::now() > deadline {
            let _ = ws.close(None);
            return Err("timed out waiting for 'ready;'".into());
        }
        match ws.read() {
            Ok(Message::Text(t)) => {
                for (cmd, args) in protocol::parse_status(t.as_str()) {
                    match cmd.as_str() {
                        "device" => device = args,
                        "protocol" => protocol = args,
                        "iq_samplerate" if iq_rate.is_empty() => iq_rate = args,
                        "ready" => {
                            let _ = ws.close(None);
                            let mut s =
                                if device.is_empty() { "connected".to_string() } else { device };
                            if !protocol.is_empty() {
                                s = format!("{s}  [{protocol}]");
                            }
                            if !iq_rate.is_empty() {
                                s = format!("{s}, IQ {iq_rate} Hz");
                            }
                            return Ok(s);
                        }
                        _ => {}
                    }
                }
            }
            Ok(Message::Close(_)) => return Err("server closed the connection".into()),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(format!("read error: {e}")),
        }
    }
}
