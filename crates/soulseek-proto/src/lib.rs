//! Sans-IO implementation of the Soulseek (slsk) wire protocol.
//!
//! The Soulseek protocol is proprietary; there is no official specification.
//! This crate follows the community reference reverse-engineered by the
//! Nicotine+ project: <https://nicotine-plus.org/doc/SLSKPROTOCOL.html>.
//!
//! Everything here is sans-IO: functions operate on byte slices and `Vec<u8>`
//! so the crate can sit under any transport (std TCP, tokio, test harnesses)
//! without depending on one.
//!
//! Wire format summary (all integers little-endian):
//! - Every message is framed as `[u32 length][code][contents]`, where
//!   `length` counts the code plus contents.
//! - Server and peer messages use a `u32` code; peer-init and distributed
//!   messages use a `u8` code.
//! - Strings are `[u32 length][bytes]`. Modern clients send UTF-8 but legacy
//!   clients sent Latin-1, so decoding falls back to Latin-1 (see [`wire`]).
//!
//! Modules:
//! - [`wire`] — primitive encode/decode (integers, strings, booleans, IPs).
//! - [`frame`] — message framing: splitting frames off a receive buffer and
//!   wrapping payloads for sending.
//! - [`server`] — client ↔ server messages (Login, SetWaitPort, ...).
//! - [`peer`] — peer-init messages (PeerInit, PierceFirewall).
//! - [`peer_message`] — peer messages over an established connection
//!   (SharedFileList / browse).

pub mod frame;
pub mod peer;
pub mod peer_message;
pub mod server;
pub mod wire;

pub use frame::{split_frame, FrameError, MAX_FRAME_LEN};
pub use wire::{DecodeError, Reader};
