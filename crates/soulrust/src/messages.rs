//! Every message that travels over the bus, plus the handler/message id
//! registries. All components spell message types from this module, and the
//! routing table in [`crate::wiring`] is keyed on the ids defined here.
//!
//! Request/response pairs carry a `corr` correlation id allocated by the web
//! bridge, which holds the reply channel for it (see
//! [`crate::components::web_bridge`]).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::extract::{Job, SearchJob};

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
    }
);

// ---------------------------------------------------------------------------
// web bridge <-> ui

/// A page or htmx fragment the UI component can render.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Page {
    Index,
    StatusFragment,
    SearchesFragment,
    ConfigForm,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpRender {
    pub corr: u64,
    pub page: Page,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpHtml {
    pub corr: u64,
    pub html: String,
}

// ---------------------------------------------------------------------------
// web bridge <-> extractor

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractRequest {
    pub corr: u64,
    pub input: String,
}

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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StartedSearch {
    pub token: u32,
    pub query: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartSearchResult {
    pub corr: u64,
    pub started: Vec<StartedSearch>,
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// web bridge <-> config store

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetConfigReq {
    pub corr: u64,
}

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetConfigResult {
    pub corr: u64,
    pub result: Result<(), String>,
}

/// Broadcast after a successful SetConfig so components refresh their copy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigChanged {
    pub config: Config,
}

// ---------------------------------------------------------------------------
// updater

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum UpdaterStatus {
    Checking,
    UpToDate { current: String },
    Available { latest: String },
    Downloading { latest: String },
    /// Downloaded but waiting for a manual apply (auto_apply = false).
    ReadyToApply { latest: String },
    RestartRequired { latest: String },
    Failed { error: String },
    Skipped { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdaterStatusChanged {
    pub status: UpdaterStatus,
}

/// Sent by the updater's background check thread to itself once a release
/// asset is fully downloaded; the handler decides whether to apply it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateDownloaded {
    pub latest: String,
    pub artifact: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyUpdateReq {
    pub corr: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyUpdateResult {
    pub corr: u64,
    pub result: Result<(), String>,
}

// ---------------------------------------------------------------------------
// session events (broadcast to the UI)

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SessionEventKind {
    Connecting,
    LoggedIn { greeting: String, own_ip: String },
    LoginFailed { reason: String },
    SearchStarted { token: u32, query: String },
    SearchBroadcastSeen { username: String, query: String },
    Disconnected { reason: String },
    ProtocolNote { note: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEvent {
    pub kind: SessionEventKind,
}

// ---------------------------------------------------------------------------
// network edge <-> session

/// One decoded-frame payload (message code + contents, no length prefix) as
/// produced by `soulseek_proto::frame::split_frame`. The session decodes it;
/// the edge only does framing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetRx {
    pub payload: Vec<u8>,
}

/// A complete outgoing wire frame for the server socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetTx {
    pub frame: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum NetConnEvent {
    Connected,
    Failed { reason: String },
    Closed { reason: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetConn {
    pub event: NetConnEvent,
}

// ---------------------------------------------------------------------------
// peer browse (look at another user's shared files)
//
// Per the bus discipline, these carry *locations* (the peer's ip/port, the
// virtual paths of shared files) and lightweight metadata (sizes) — never file
// contents. A downloader, when added, will write bytes to disk and put only the
// resulting path on the bus, exactly as the updater does with `artifact`.

/// web bridge → session: begin browsing `username`'s shares.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowseUser {
    pub corr: u64,
    pub username: String,
}

/// session → web bridge: whether the browse request was accepted (it can be
/// rejected immediately, e.g. when not logged in). Success here only means the
/// lookup started; the listing arrives later for the UI to poll.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowseAccepted {
    pub corr: u64,
    pub error: Option<String>,
}

/// session → peer edge: the resolved address to open a peer connection to.
/// A location, not data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerBrowseConnect {
    pub username: String,
    pub ip: String,
    pub port: u16,
}

/// One shared file as shown in the browse view: its name and size only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrowseFile {
    pub name: String,
    pub size: u64,
}

/// One shared directory: its virtual path and the files within it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrowseDir {
    pub path: String,
    pub files: Vec<BrowseFile>,
}

/// peer edge → browse: the fetched share tree (paths + sizes). `truncated` is
/// set when the peer's list was larger than the cap the edge forwards, so the
/// UI can say so rather than silently implying completeness.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowseListing {
    pub username: String,
    pub directories: Vec<BrowseDir>,
    pub total_files: u64,
    pub truncated: bool,
}

/// peer edge / session → browse: the browse could not be completed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowseFailed {
    pub username: String,
    pub reason: String,
}

/// web bridge → browse: render the browse fragment for the current state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowseRenderReq {
    pub corr: u64,
}

/// browse → web bridge: the rendered browse fragment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowseHtml {
    pub corr: u64,
    pub html: String,
}

// ---------------------------------------------------------------------------
// peer network edge → ui (serving activity, shown in the log)

/// peer_net → ui: a notable serving event (a peer connected, we served a
/// browse, the listener bound, …). Just a log line — no bulk data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerActivity {
    pub note: String,
}

/// session → peer_net: a search relayed by the server. peer_net matches it
/// against our shares and, on a hit, delivers a FileSearchResponse to the
/// searcher (queue + ConnectToPeer relay).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncomingSearch {
    pub username: String,
    pub token: u32,
    pub query: String,
}

/// session → peer_net: a server ConnectToPeer — a (likely firewalled) peer
/// wants to reach us. We connect to `ip:port` and send PierceFirewall(token).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerPierce {
    pub username: String,
    pub ip: String,
    pub port: u16,
    pub token: u32,
}

// ---------------------------------------------------------------------------
// downloads (request a file from a peer)

/// ui / web bridge → session: start downloading `filename` (a peer's virtual
/// path) from `username`. `size` comes from the search/browse result the user
/// picked, so we know how many bytes to expect.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartDownload {
    pub username: String,
    pub filename: String,
    pub size: u64,
}

/// session → peer_net: the resolved address to open a peer connection to and
/// queue a download. A location, not data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerDownloadConnect {
    pub username: String,
    pub ip: String,
    pub port: u16,
    pub filename: String,
    pub size: u64,
}

/// peer_net → ui: a download finished and was moved to its final path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadComplete {
    pub username: String,
    pub filename: String,
    pub path: String,
}

/// peer_net → ui: a download could not be completed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadFailed {
    pub username: String,
    pub filename: String,
    pub reason: String,
}

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
    HttpRender => MessageId::HttpRender,
    HttpHtml => MessageId::HttpHtml,
    ExtractRequest => MessageId::ExtractRequest,
    ExtractResult => MessageId::ExtractResult,
    StartSearch => MessageId::StartSearch,
    StartSearchResult => MessageId::StartSearchResult,
    GetConfigReq => MessageId::GetConfigReq,
    ConfigSnapshot => MessageId::ConfigSnapshot,
    SetConfigReq => MessageId::SetConfigReq,
    SetConfigResult => MessageId::SetConfigResult,
    ConfigChanged => MessageId::ConfigChanged,
    UpdaterStatusChanged => MessageId::UpdaterStatusChanged,
    UpdateDownloaded => MessageId::UpdateDownloaded,
    ApplyUpdateReq => MessageId::ApplyUpdateReq,
    ApplyUpdateResult => MessageId::ApplyUpdateResult,
    SessionEvent => MessageId::SessionEvent,
    NetRx => MessageId::NetRx,
    NetTx => MessageId::NetTx,
    NetConn => MessageId::NetConn,
    BrowseUser => MessageId::BrowseUser,
    BrowseAccepted => MessageId::BrowseAccepted,
    PeerBrowseConnect => MessageId::PeerBrowseConnect,
    BrowseListing => MessageId::BrowseListing,
    BrowseFailed => MessageId::BrowseFailed,
    BrowseRenderReq => MessageId::BrowseRenderReq,
    BrowseHtml => MessageId::BrowseHtml,
    PeerActivity => MessageId::PeerActivity,
    IncomingSearch => MessageId::IncomingSearch,
    PeerPierce => MessageId::PeerPierce,
    StartDownload => MessageId::StartDownload,
    PeerDownloadConnect => MessageId::PeerDownloadConnect,
    DownloadComplete => MessageId::DownloadComplete,
    DownloadFailed => MessageId::DownloadFailed,
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
        let msg = NetConn { event: NetConnEvent::Connected };
        let size = msg.get_size();
        let mut buf = vec![0u8; size + 7];
        msg.write_into(&mut buf[..size]);
        assert_eq!(NetConn::deserialize_from(&buf), msg);
    }
}
