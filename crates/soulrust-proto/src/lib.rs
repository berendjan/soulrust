//! Generated protobuf wire types (buffa) and Connect service stubs
//! (connectrpc), assembled by Bazel. The `#[path = ...]` mounts point at files
//! produced by the `//crates/soulrust-proto:gen_greet` codegen action; they
//! live in the Bazel output tree and are wired into the `rust_library` as srcs.
//!
//! Stage 0 carries only the `greet` spike. Real packages (`soulrust.bus.v1`,
//! `soulrust.api.v1`) are added in later stages.

#[path = "../generated/buffa/mod.rs"]
pub mod proto;

#[path = "../generated/connect/mod.rs"]
pub mod connect;

#[cfg(test)]
mod tests {
    use super::*;
    use buffa::{Message, MessageView};

    #[test]
    fn greet_messages_round_trip_through_buffa() {
        let original = proto::soulrust::greet::v1::GreetResponse {
            message: "Hello, world!".into(),
            ..Default::default()
        };
        let bytes = original.encode_to_vec();
        let decoded = proto::soulrust::greet::v1::GreetResponse::decode_from_slice(&bytes)
            .expect("decode round-trip");
        assert_eq!(decoded.message, "Hello, world!");
    }

    #[test]
    fn greet_view_decodes_zero_copy_over_the_wire() {
        let original = proto::soulrust::greet::v1::GreetRequest {
            name: "berend".into(),
            ..Default::default()
        };
        let bytes = original.encode_to_vec();
        // The whole point for the bus: read a borrowed view straight off the
        // encoded bytes, no owned allocation.
        let view = proto::soulrust::greet::v1::GreetRequestView::decode_view(&bytes)
            .expect("view decode");
        assert_eq!(view.name, "berend");
    }

    #[test]
    fn connect_service_name_constant_is_correct() {
        assert_eq!(
            connect::soulrust::greet::v1::GREET_SERVICE_SERVICE_NAME,
            "soulrust.greet.v1.GreetService"
        );
    }
}
