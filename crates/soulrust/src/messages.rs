//! Every message that travels over the bus, plus the handler/message id
//! registries. All components spell message types from this module, and the
//! routing table in [`crate::wiring`] is keyed on the ids defined here.
//!
//! Request/response pairs carry a `corr` correlation id allocated by the web
//! bridge, which holds the reply channel for it (see
//! [`crate::components::web_bridge`]).


use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::extract::{Job, SearchJob};

// The bus id registries (`HandlerId`/`MessageId`) live in `soulrust-proto` so
// the buffa bus-type trait impls there satisfy the orphan rule. Re-exported here
// to preserve the original `crate::messages::{HandlerId, MessageId}` paths.
pub use soulrust_proto::{EnumValue, HandlerId, MessageField, MessageId};

// Buffa bus message + enum types migrated from this module.
pub use soulrust_proto::bus::HttpRender;

pub use soulrust_proto::bus::{
    ApplyUpdateResult, NetConn, NetConnKind, SessionEvent, SessionEventKind, SetConfigResult,
    UpdaterStatusChanged, UpdaterStatusKind,
};

// ---------------------------------------------------------------------------
// web bridge <-> ui

/// A page or htmx fragment the UI component can render.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Page {
    Index,
    StatusFragment,
    SearchesFragment,
    ConfigForm,
    /// The login/signup screen's live connection state (friendly, no log).
    AccountStatus,
    /// The Downloads page (full shell) and its live fragment listing active +
    /// finished transfers.
    Downloads,
    DownloadsFragment,
    /// The Uploads monitor page (full shell) and its live fragment listing
    /// active + finished uploads served to peers.
    Uploads,
    UploadsFragment,
    /// Set the results-table sort column (toggles direction if already active),
    /// then render the searches fragment. `key` is a column id (user/folder/
    /// file/size/bitrate/length/slot/speed/queue).
    SortSearches { key: String },
    /// Set the minimum-bitrate filter (kbps; 0 clears it), then render the
    /// searches fragment.
    FilterBitrate { min: u32 },
}

/// Map the rich `Page` to the flat buffa `Page` carried in `HttpRender`.
pub fn page_to_proto(p: &Page) -> soulrust_proto::bus::Page {
    use soulrust_proto::bus::PageKind as K;
    let (kind, sort_key, min_bitrate) = match p {
        Page::Index => (K::PageIndex, String::new(), 0),
        Page::StatusFragment => (K::PageStatusFragment, String::new(), 0),
        Page::SearchesFragment => (K::PageSearchesFragment, String::new(), 0),
        Page::ConfigForm => (K::PageConfigForm, String::new(), 0),
        Page::AccountStatus => (K::PageAccountStatus, String::new(), 0),
        Page::Downloads => (K::PageDownloads, String::new(), 0),
        Page::DownloadsFragment => (K::PageDownloadsFragment, String::new(), 0),
        Page::Uploads => (K::PageUploads, String::new(), 0),
        Page::UploadsFragment => (K::PageUploadsFragment, String::new(), 0),
        Page::SortSearches { key } => (K::PageSortSearches, key.clone(), 0),
        Page::FilterBitrate { min } => (K::PageFilterBitrate, String::new(), *min),
    };
    soulrust_proto::bus::Page { kind: kind.into(), sort_key, min_bitrate, ..Default::default() }
}

/// Reverse of [`page_to_proto`].
pub fn page_from_proto(p: &soulrust_proto::bus::Page) -> Page {
    use soulrust_proto::bus::PageKind as K;
    match p.kind {
        EnumValue::Known(K::PageStatusFragment) => Page::StatusFragment,
        EnumValue::Known(K::PageSearchesFragment) => Page::SearchesFragment,
        EnumValue::Known(K::PageConfigForm) => Page::ConfigForm,
        EnumValue::Known(K::PageAccountStatus) => Page::AccountStatus,
        EnumValue::Known(K::PageDownloads) => Page::Downloads,
        EnumValue::Known(K::PageDownloadsFragment) => Page::DownloadsFragment,
        EnumValue::Known(K::PageUploads) => Page::Uploads,
        EnumValue::Known(K::PageUploadsFragment) => Page::UploadsFragment,
        EnumValue::Known(K::PageSortSearches) => Page::SortSearches { key: p.sort_key.clone() },
        EnumValue::Known(K::PageFilterBitrate) => Page::FilterBitrate { min: p.min_bitrate },
        _ => Page::Index,
    }
}
// ---------------------------------------------------------------------------
// web bridge <-> extractor
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractResult {
    pub corr: u64,
    pub result: Result<Job, String>,
}

// ---------------------------------------------------------------------------
// web bridge <-> session

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartSearch {
    pub corr: u64,
    pub source_label: String,
    pub jobs: Vec<SearchJob>,
}
// ---------------------------------------------------------------------------
// web bridge <-> config store
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigSnapshot {
    pub corr: u64,
    pub config: Config,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetConfigReq {
    pub corr: u64,
    pub config: Config,
}
/// Broadcast after a successful SetConfig so components refresh their copy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigChanged {
    pub config: Config,
}
// ---------------------------------------------------------------------------
// peer network edge → ui (serving activity, shown in the log)

// Buffa bus messages (wire types + bus-trait bridge live in soulrust-proto).
// Re-exported here so components keep importing them from `crate::messages`.
pub use soulrust_proto::bus::{
    ApplyUpdateReq, BrowseAccepted, BrowseDir, BrowseFailed, BrowseFile, BrowseHtml, BrowseListing,
    BrowseRenderReq, BrowseUser, CancelDownload, DistribSpeedLimits, DownloadComplete,
    DownloadFailed, DownloadQueuePosition, ExtractRequest, GetConfigReq, HttpHtml, IncomingSearch,
    NetRx, NetTx, PauseDownload, PeerActivity, PeerBrowseConnect, PeerDistribConnect, PeerDownloadConnect,
    PeerPierce, PeerPierceDistrib, PeerPierceFile, PeerUploadConnect, RelayDistribSearch,
    ResolveUploadPeer, SearchResultFile, SearchResultReceived, SetExcludedPhrases, StartDownload,
    StartSearchResult, StartedSearch, TransferProgress, UpdateDownloaded, UploadComplete, UploadFailed,
    UploadStarted,
};
// ---------------------------------------------------------------------------
// distributed search network
// ---------------------------------------------------------------------------
// bus plumbing: Message + ExtendedMessage + deserialize_from for every type

macro_rules! impl_bus_message {
    ($($type:ty => $id:expr),+ $(,)?) => {
        $(
            impl rust_messenger::traits::core::Message for $type {
                type Id = MessageId;
                const ID: MessageId = $id;
            }

            impl $type {
                pub fn deserialize_from(buffer: &[u8]) -> Self {
                    bincode::serde::borrow_decode_from_slice(buffer, bincode::config::standard())
                        .expect("bus message failed to decode; sender/receiver out of sync")
                        .0
                }
            }

            impl rust_messenger::traits::extended::ExtendedMessage for $type {
                fn get_size(&self) -> usize {
                    bincode::serde::encode_to_vec(self, bincode::config::standard())
                        .expect("bus message failed to encode")
                        .len()
                }

                fn write_into(&self, buffer: &mut [u8]) {
                    bincode::serde::encode_into_slice(self, buffer, bincode::config::standard())
                        .expect("bus message failed to encode");
                }
            }
        )+
    };
}

impl_bus_message!(
    ExtractResult => MessageId::ExtractResult,
    StartSearch => MessageId::StartSearch,
    ConfigSnapshot => MessageId::ConfigSnapshot,
    SetConfigReq => MessageId::SetConfigReq,
    ConfigChanged => MessageId::ConfigChanged,
);

#[cfg(test)]
mod tests {
    use super::*;
    use rust_messenger::traits::extended::ExtendedMessage;

    #[test]
    fn messages_round_trip_through_bincode() {
        let msg = StartSearch {
            corr: 7,
            source_label: "spotify: Test Playlist".into(),
            jobs: vec![SearchJob {
                artist: Some("Artist".into()),
                title: Some("Title".into()),
                album: None,
                raw_query: None,
            }],
        };
        let mut buf = vec![0u8; msg.get_size()];
        msg.write_into(&mut buf);
        let back = StartSearch::deserialize_from(&buf);
        assert_eq!(back.corr, 7);
        assert_eq!(back.jobs.len(), 1);
        assert_eq!(back.jobs[0].artist.as_deref(), Some("Artist"));
    }

    #[test]
    fn decode_tolerates_aligned_tail_padding() {
        // The bus hands deserialize_from a payload padded to usize alignment;
        // trailing zeroes must not break decoding.
        let msg = NetConn {
            kind: NetConnKind::NetConnFailed.into(),
            reason: "boom".into(),
            ..Default::default()
        };
        let size = msg.get_size();
        let mut buf = vec![0u8; size + 7];
        msg.write_into(&mut buf[..size]);
        assert_eq!(NetConn::deserialize_from(&buf), msg);
    }
}
