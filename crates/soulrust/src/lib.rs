//! Soulseek client built as nano-services on the rust-messenger bus.
//!
//! Components are synchronous `&mut self` handlers owned by worker threads;
//! they communicate only through bus messages (see [`messages`]). I/O lives
//! at the edges: [`components::web_bridge`] serves the htmx UI over
//! tiny_http worker threads, [`components::net_edge`] owns the Soulseek
//! server socket. Request/response across the bus uses correlation ids with
//! reply channels held by the web bridge.
//!
//! The routing table — which component receives which message from which
//! source — lives in [`wiring`].

pub mod components;
pub mod config;
pub mod extract;
pub mod messages;
pub mod search_response;
pub mod shares;
pub mod transfers;
pub mod version;
pub mod wiring;
