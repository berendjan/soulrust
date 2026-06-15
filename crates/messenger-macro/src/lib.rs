//! Rust port of the Go `interface-gen-v2` + `messenger-gen-v2` codegen tools.
//!
//! Instead of a YAML spec processed by an external generator, the component
//! topology is declared inline with the [`messenger!`] macro and the code is
//! generated at compile time. Spec validation errors (the checks the Go tools
//! performed in `Validate()`) surface as compile errors on the offending line.
//!
//! ```ignore
//! messenger! {
//!     messenger UnitMessenger;
//!
//!     component controller {
//!         sends GetTenantRequest -> GetTenantResponse; // 1:1, exactly one handler required
//!         sends TenantUpdated;                         // event, 0..N handlers (fan-out)
//!         handles WorkerDone;
//!     }
//!
//!     component worker {
//!         handles GetTenantRequest -> GetTenantResponse;
//!         handles TenantUpdated;
//!         sends WorkerDone;
//!     }
//! }
//! ```
//!
//! For each component this generates (mirroring the Go output):
//! - `<Name>Sendable` trait — the `send_*` methods the component may call.
//! - `<Name>Handler` trait — the `handle_*` methods the component must implement.
//! - A `UnitMessenger` struct that owns all handling components and routes each
//!   send to its handler(s): by value for 1:1 request/response, cloned fan-out
//!   for events.
//!
//! Components obtain their sendable through `messenger.<name>_sendable()`,
//! which returns a short-lived view implementing `<Name>Sendable` — the Rust
//! equivalent of the Go `SetSendable` wiring without the parent pointers.

mod generate;
mod spec;

use proc_macro::TokenStream;
use syn::parse_macro_input;

#[proc_macro]
pub fn messenger(input: TokenStream) -> TokenStream {
    let spec = parse_macro_input!(input as spec::Spec);
    match spec
        .validate()
        .and_then(|()| generate::generate(&spec))
    {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}
