use serde::{Deserialize, Serialize};

/// WSJT-X UDP broadcast configuration (`wsjtx.json`).
///
/// This is sdroxide *being* WSJT-X for the logging ecosystem: GridTracker,
/// JTAlert, N1MM+ and Log4OM all learn about decodes and contacts from the
/// datagrams WSJT-X sends to UDP 2237. It complements [`crate::RigctldConfig`]
/// and [`crate::TciServerConfig`], which offer control surfaces — this one is
/// output only, and nothing on the socket can touch the radio.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WsjtxConfig {
    /// Off by default: broadcasting where the station is and who it works is
    /// the operator's decision, even on the loopback interface.
    pub enabled: bool,
    /// Where to send. `127.0.0.1` reaches clients on this machine; a LAN
    /// address or a multicast group (`224.0.0.1`) reaches others.
    pub host: String,
    /// 2237 is the port every client defaults to.
    pub port: u16,
    /// The name clients see. Some loggers only accept traffic identifying
    /// itself as `WSJT-X`, which is why that — and not `sdroxide` — is the
    /// default.
    pub id: String,
}

impl Default for WsjtxConfig {
    fn default() -> Self {
        WsjtxConfig { enabled: false, host: "127.0.0.1".into(), port: 2237, id: "WSJT-X".into() }
    }
}

impl WsjtxConfig {
    pub fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}
