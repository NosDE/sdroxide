//! Native client for Icom's network protocol — the one an RS-BA1 server speaks,
//! as used by the IC-705, IC-7610 and IC-9700 over LAN or WLAN.
//!
//! NATIVE ONLY. Plain UDP sockets; this crate must never be a dependency of any
//! wasm-targeted crate.
//!
//! The radio offers three UDP streams, each opened with the same handshake and
//! each carrying its own sequence numbers:
//!
//! * **control** (50001) — login, the session token, and the request that opens
//!   the other two,
//! * **serial** (50002) — CI-V frames, the network stand-in for the CAT cable.
//!   The framing is shared with [`sdroxide_cat::civ`]: same commands, different
//!   transport,
//! * **audio** (50003) — receive and transmit audio, 16-bit PCM.
//!
//! Unlike a FlexRadio, an Icom sends no IQ, so this backend feeds the engine's
//! audio-band path (`DeviceCaps::audio_mode`) exactly as the serial CAT backend
//! does — what it removes is the cable and the sound card, not the bandwidth
//! limit.

mod net;

pub mod control;
pub mod packet;
pub mod payload;
pub mod scope;
pub mod stream;

pub use net::{AUDIO_RATE_HZ, Connect, IcomHandle, IcomUpdate};
pub use scope::{SPANS_HZ, Sweep};

pub use stream::{IcomError, Result};
