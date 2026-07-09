//! Soulseek client built as nano-services on the rust-messenger bus.
//!
//! Components are synchronous `&mut self` handlers owned by worker threads;
//! they communicate only through bus messages (see [`messages`]). I/O lives
//! at the edges: [`components::api_server`] serves the Connect API + React UI
//! over axum, [`components::net_edge`] owns the Soulseek server socket.
//! Request/response across the bus uses correlation ids with reply channels
//! held by the API edge.
//!
//! The routing table — which component receives which message from which
//! source — lives in [`wiring`].

#[cfg(cargo_build)]
extern crate tokio as tokio_api;

pub mod components;
pub mod config;
// The Vite React bundle, embedded as a `(path, bytes)` table and served by
// [`components::api_server`]. Bazel supplies the real generated module;
// Cargo gets an empty fallback from build.rs so `cargo check` remains usable.
#[cfg(cargo_build)]
mod web_assets_gen {
    include!(concat!(env!("OUT_DIR"), "/web_assets_gen.rs"));
}
pub mod extract;
pub mod messages;
pub mod search_response;
pub mod shares;
pub mod transfers;
pub mod version;
#[cfg(not(cargo_build))]
mod web_assets_gen;
pub mod wiring;
