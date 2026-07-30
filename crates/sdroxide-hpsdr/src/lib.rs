//! Native OpenHPSDR (ethernet SDR) support — **Protocol 1 and Protocol 2**.
//!
//! NATIVE ONLY. Pure-Rust UDP; this crate must never be a dependency of any
//! wasm-targeted crate (mirrors the `sdroxide-cat` invariant). It is reached
//! only from the root binary and `local_controller.rs`; the settings UI talks to
//! it exclusively through the `RadioController` trait.
//!
//! Discovery probes for both protocols and each board is driven by the one it
//! answers on: Protocol 1 (Metis framing — Hermes-Lite 2 and the legacy
//! Metis/Hermes boards) or Protocol 2. The two live in sibling framing modules
//! behind the same discovery + [`HpsdrHandle`] abstraction.

mod discovery;
mod net;
mod protocol1;
mod protocol2;

use std::time::Duration;

pub use discovery::{discover, probe};
pub use net::{
    HpsdrError, HpsdrHandle, LNA_GAIN_DEFAULT_DB, LNA_GAIN_ELEMENT, LNA_GAIN_MAX_DB,
    LNA_GAIN_MIN_DB, TX_RATE_HZ, board_has_lna_gain,
};
pub use sdroxide_types::HpsdrDevice;

/// Convenience: broadcast-scan the LAN with a default 1.5 s timeout.
pub fn discover_default() -> Vec<HpsdrDevice> {
    discover(Duration::from_millis(1500))
}
