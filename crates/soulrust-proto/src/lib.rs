//! soulrust's protobuf surface, assembled by Bazel.
//!
//! - `bus`: buffa wire types for the rust-messenger message bus, bridged to the
//!   bus's `Message`/`ExtendedMessage` traits below. Migrated from the
//!   hand-written structs in `crates/soulrust/src/messages.rs`, one batch at a
//!   time; `soulrust` re-exports these so component imports don't churn.
//! - `proto` / `connect`: the Stage-0 greet spike (buffa types + a connectrpc
//!   service stub). Retained as a smoke test until the real API package lands.
//!
//! The `MessageId`/`HandlerId` registries live here (not in `soulrust`) so the
//! bus-trait impls for the generated buffa types satisfy the orphan rule — both
//! the trait's `Id` type and the impl'd-on type are local to this crate.

// --- Stage-0 greet spike (buffa + connect), kept as a smoke test ------------
#[path = "../generated/buffa/mod.rs"]
pub mod proto;

#[path = "../generated/connect/mod.rs"]
pub mod connect;

// --- Bus wire types ---------------------------------------------------------
#[path = "../generated/bus/mod.rs"]
mod bus_gen;

/// The bus message payloads (package `soulrust.bus.v1`).
pub use bus_gen::soulrust::bus::v1 as bus;

/// Buffa value wrappers, re-exported so `soulrust` can match generated enum
/// fields (`EnumValue<Kind>`) and optional sub-messages without depending on
/// buffa directly.
pub use buffa::{EnumValue, MessageField};

// --- Public Connect API (soulrust.api.v1) -----------------------------------
// `api` holds the buffa message types; the connectrpc service stubs in
// `api_connect` reference them as `crate::api::...` (buffa_module=crate::api).
#[path = "../generated/api_buffa/mod.rs"]
pub mod api;

#[path = "../generated/api_connect/mod.rs"]
pub mod api_connect;

// --- Bus registries (moved here from messages.rs) ---------------------------
rust_messenger::messenger_id_enum!(
    HandlerId {
        Session = 1,
        ConfigStore = 2,
        Updater = 3,
        Ui = 4,
        NetEdge = 5,
        WebBridge = 6,
        Extractor = 7,
        PeerEdge = 8,
        Browse = 9,
        PeerNet = 10,
        ApiServer = 11,
    }
);

rust_messenger::messenger_id_enum!(
    MessageId {
        HttpRender = 1,
        HttpHtml = 2,
        ExtractRequest = 3,
        ExtractResult = 4,
        StartSearch = 5,
        StartSearchResult = 6,
        GetConfigReq = 7,
        ConfigSnapshot = 8,
        SetConfigReq = 9,
        SetConfigResult = 10,
        ConfigChanged = 11,
        UpdaterStatusChanged = 12,
        UpdateDownloaded = 13,
        ApplyUpdateReq = 14,
        ApplyUpdateResult = 15,
        SessionEvent = 16,
        NetRx = 17,
        NetTx = 18,
        NetConn = 19,
        BrowseUser = 20,
        BrowseAccepted = 21,
        PeerBrowseConnect = 22,
        BrowseListing = 23,
        BrowseFailed = 24,
        BrowseRenderReq = 25,
        BrowseHtml = 26,
        PeerActivity = 27,
        IncomingSearch = 28,
        PeerPierce = 29,
        StartDownload = 30,
        PeerDownloadConnect = 31,
        DownloadComplete = 32,
        DownloadFailed = 33,
        ResolveUploadPeer = 34,
        PeerUploadConnect = 35,
        UploadComplete = 36,
        UploadFailed = 37,
        PeerDistribConnect = 38,
        SetExcludedPhrases = 39,
        DownloadQueuePosition = 40,
        SearchResultReceived = 41,
        CancelDownload = 42,
        PeerPierceFile = 43,
        PeerPierceDistrib = 44,
        RelayDistribSearch = 45,
        DistribSpeedLimits = 46,
        UploadStarted = 47,
        TransferProgress = 48,
        PauseDownload = 49,
    }
);

/// Byte length of `v` encoded as a protobuf base-128 varint.
pub fn varint_len(mut v: u64) -> usize {
    let mut n = 1;
    while v >= 0x80 {
        v >>= 7;
        n += 1;
    }
    n
}

/// Bridge a buffa message type onto the rust-messenger bus traits.
///
/// The bus hands `deserialize_from` a slice padded to `usize` alignment, which
/// raw protobuf can't tolerate (a trailing zero byte reads as field 0). We use
/// buffa's **length-delimited** framing — a varint length prefix then the body —
/// so decode reads exactly the body and ignores the padding, mirroring what the
/// old bincode framing did via its length-delimited encoding.
macro_rules! impl_bus_buffa {
    ($($type:ty => $id:expr),+ $(,)?) => {
        $(
            impl ::rust_messenger::traits::core::Message for $type {
                type Id = $crate::MessageId;
                const ID: $crate::MessageId = $id;
            }

            impl $type {
                pub fn deserialize_from(buffer: &[u8]) -> Self {
                    <Self as ::buffa::Message>::decode_length_delimited(&mut &buffer[..])
                        .expect("bus message failed to decode; sender/receiver out of sync")
                }
            }

            impl ::rust_messenger::traits::extended::ExtendedMessage for $type {
                fn get_size(&self) -> usize {
                    let len = ::buffa::Message::encoded_len(self);
                    $crate::varint_len(len as u64) + len as usize
                }

                fn write_into(&self, buffer: &mut [u8]) {
                    let mut buf: &mut [u8] = buffer;
                    ::buffa::Message::encode_length_delimited(self, &mut buf);
                }
            }
        )+
    };
}

/// Bridge a buffa **owned-view** type (`FooOwnedView` = a zero-copy view over a
/// backing `Bytes`) onto the bus. Used for large read-mostly messages so the
/// receiving read-model holds a view — the payload is copied once into `Bytes`
/// on decode and never re-allocated field-by-field, and `clone()` is an O(1)
/// refcount bump rather than a deep copy. Framed with a u32-LE length prefix so
/// decode reads exactly the body and ignores the ring's alignment padding.
macro_rules! impl_bus_buffa_owned {
    ($($type:ty => $id:expr),+ $(,)?) => {
        $(
            impl ::rust_messenger::traits::core::Message for $type {
                type Id = $crate::MessageId;
                const ID: $crate::MessageId = $id;
            }

            impl $type {
                pub fn deserialize_from(buffer: &[u8]) -> Self {
                    let len = u32::from_le_bytes(
                        buffer[..4].try_into().expect("bus owned-view: short length prefix"),
                    ) as usize;
                    let body = ::buffa::bytes::Bytes::copy_from_slice(&buffer[4..4 + len]);
                    <$type>::decode(body)
                        .expect("bus message failed to decode; sender/receiver out of sync")
                }
            }

            impl ::rust_messenger::traits::extended::ExtendedMessage for $type {
                fn get_size(&self) -> usize {
                    4 + self.bytes().len()
                }

                fn write_into(&self, buffer: &mut [u8]) {
                    let body = self.bytes();
                    buffer[..4].copy_from_slice(&(body.len() as u32).to_le_bytes());
                    buffer[4..4 + body.len()].copy_from_slice(body);
                }
            }
        )+
    };
}

impl_bus_buffa_owned!(
    bus::BrowseListingOwnedView => MessageId::BrowseListing,
);

impl_bus_buffa!(
    bus::PeerActivity => MessageId::PeerActivity,
    bus::HttpHtml => MessageId::HttpHtml,
    bus::ExtractRequest => MessageId::ExtractRequest,
    bus::GetConfigReq => MessageId::GetConfigReq,
    bus::ApplyUpdateReq => MessageId::ApplyUpdateReq,
    bus::BrowseUser => MessageId::BrowseUser,
    bus::BrowseAccepted => MessageId::BrowseAccepted,
    bus::BrowseFailed => MessageId::BrowseFailed,
    bus::BrowseRenderReq => MessageId::BrowseRenderReq,
    bus::BrowseHtml => MessageId::BrowseHtml,
    bus::IncomingSearch => MessageId::IncomingSearch,
    bus::SetExcludedPhrases => MessageId::SetExcludedPhrases,
    bus::RelayDistribSearch => MessageId::RelayDistribSearch,
    bus::DistribSpeedLimits => MessageId::DistribSpeedLimits,
    bus::StartDownload => MessageId::StartDownload,
    bus::CancelDownload => MessageId::CancelDownload,
    bus::PauseDownload => MessageId::PauseDownload,
    bus::DownloadComplete => MessageId::DownloadComplete,
    bus::DownloadFailed => MessageId::DownloadFailed,
    bus::DownloadQueuePosition => MessageId::DownloadQueuePosition,
    bus::ResolveUploadPeer => MessageId::ResolveUploadPeer,
    bus::UploadStarted => MessageId::UploadStarted,
    bus::TransferProgress => MessageId::TransferProgress,
    bus::UploadComplete => MessageId::UploadComplete,
    bus::UploadFailed => MessageId::UploadFailed,
    bus::StartSearchResult => MessageId::StartSearchResult,
    bus::SearchResultReceived => MessageId::SearchResultReceived,
    bus::PeerBrowseConnect => MessageId::PeerBrowseConnect,
    bus::PeerPierce => MessageId::PeerPierce,
    bus::PeerPierceFile => MessageId::PeerPierceFile,
    bus::PeerPierceDistrib => MessageId::PeerPierceDistrib,
    bus::PeerDownloadConnect => MessageId::PeerDownloadConnect,
    bus::PeerUploadConnect => MessageId::PeerUploadConnect,
    bus::PeerDistribConnect => MessageId::PeerDistribConnect,
    bus::NetRx => MessageId::NetRx,
    bus::NetTx => MessageId::NetTx,
    bus::UpdateDownloaded => MessageId::UpdateDownloaded,
    bus::NetConn => MessageId::NetConn,
    bus::SessionEvent => MessageId::SessionEvent,
    bus::UpdaterStatusChanged => MessageId::UpdaterStatusChanged,
    bus::SetConfigResult => MessageId::SetConfigResult,
    bus::ApplyUpdateResult => MessageId::ApplyUpdateResult,
    bus::HttpRender => MessageId::HttpRender,
    bus::ExtractResult => MessageId::ExtractResult,
    bus::StartSearch => MessageId::StartSearch,
    bus::ConfigSnapshot => MessageId::ConfigSnapshot,
    bus::SetConfigReq => MessageId::SetConfigReq,
    bus::ConfigChanged => MessageId::ConfigChanged,
);

#[cfg(test)]
mod tests {
    use super::*;
    use buffa::{Message, MessageView};
    use rust_messenger::traits::extended::ExtendedMessage;

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

    #[test]
    fn api_status_service_generates() {
        // The real public API service stub + message types compile and carry
        // the right fully-qualified service name.
        assert_eq!(
            api_connect::soulrust::api::v1::STATUS_SERVICE_SERVICE_NAME,
            "soulrust.api.v1.StatusService"
        );
        let resp = api::soulrust::api::v1::GetStatusResponse {
            logged_in: true,
            username: "alice".into(),
            shared_files: 42,
            ..Default::default()
        };
        let bytes = resp.encode_to_vec();
        let back =
            api::soulrust::api::v1::GetStatusResponse::decode_from_slice(&bytes).unwrap();
        assert_eq!(back.username, "alice");
        assert_eq!(back.shared_files, 42);
    }

    #[test]
    fn peer_activity_bridges_the_bus_traits_with_padding_tolerance() {
        // Encode through the bus' ExtendedMessage path, then decode through
        // deserialize_from with trailing alignment padding — the exact shape
        // the rust-messenger ring buffer produces.
        let msg = bus::PeerActivity { note: "listening on 2234".into(), ..Default::default() };
        let size = msg.get_size();
        let mut buf = vec![0u8; size + 7]; // 7 bytes of alignment padding
        msg.write_into(&mut buf[..size]);
        let back = bus::PeerActivity::deserialize_from(&buf);
        assert_eq!(back.note, "listening on 2234");
    }
}
