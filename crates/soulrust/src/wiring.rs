//! The routing table: which component receives which message from which
//! source, and which worker thread each component lives on.
//!
//! Two workers:
//! - `CoreWorker` hosts everything with fast handlers (session, config,
//!   updater, the peer/net edges, and the Connect API edge).
//! - `ExtractWorker` hosts only the extractor, whose Spotify calls can block
//!   for seconds — on its own worker they never stall the core handlers.
//!
//! The Connect API edge ([`ApiServer`]) is the sole UI-facing surface: it sends
//! the UI's commands (search, download, browse, config, update) onto the bus and
//! consumes every view-relevant event to keep its read-model snapshots current.

use crate::components::api_server::ApiServer;
use crate::components::net_edge::NetEdge;
use crate::components::peer_net::PeerNet;
use crate::components::session::Session;
use crate::components::updater::Updater;
use crate::config::ConfigStore;
use crate::extract::ExtractorComponent;
use crate::messages::{
    ApplyUpdateReq, ApplyUpdateResult, BrowseAccepted, BrowseFailed, BrowseListingOwnedView,
    BrowseUser, CancelDownload, ConfigChanged, DistribSpeedLimits, DownloadComplete, DownloadFailed,
    DownloadQueuePosition, ExtractRequest, ExtractResult, IncomingSearch, NetConn, NetRx, NetTx,
    PauseDownload, PeerActivity, PeerBrowseConnect, PeerDistribConnect, PeerDownloadConnect,
    PeerPierce, PeerPierceDistrib, PeerPierceFile, PeerUploadConnect, RelayDistribSearch,
    RemoveSearch, ResolveUploadPeer, SearchResultReceived, SessionEvent, SetConfigReq,
    SetConfigResult, SetExcludedPhrases, StartDownload, StartSearch, StartSearchResult,
    TransferProgress, UpdateDownloaded, UpdaterStatusChanged, UploadComplete, UploadFailed,
    UploadStarted,
};

rust_messenger::Messenger! {
    crate::config::AppContext,
    CoreWorker:
        handlers: [
            session: Session,
            config_store: ConfigStore,
            updater: Updater,
            net_edge: NetEdge,
            peer_net: PeerNet,
            api_server: ApiServer,
        ]
        routes: [
            // api edge commands -> the components that act on them
            ApiServer, StartSearch: [ session ],
            ApiServer, SetConfigReq: [ config_store ],
            ApiServer, ApplyUpdateReq: [ updater ],
            ApiServer, BrowseUser: [ session ],
            // self-delivered so the edge records the state change (mirrors the
            // old Ui: queued/cancelled/paused/closed rows).
            ApiServer, RemoveSearch: [ api_server ],
            ApiServer, StartDownload: [ session, api_server ],
            ApiServer, CancelDownload: [ peer_net, api_server ],
            ApiServer, PauseDownload: [ peer_net, api_server ],
            // request/reply completions back to the edge
            Session, StartSearchResult: [ api_server ],
            ExtractorComponent, ExtractResult: [ api_server ],
            ConfigStore, SetConfigResult: [ api_server ],
            Updater, ApplyUpdateResult: [ api_server ],
            Session, BrowseAccepted: [ api_server ],
            // soulseek socket <-> session; peer_net also sends server frames.
            NetEdge, NetConn: [ session ],
            NetEdge, NetRx: [ session ],
            Session, NetTx: [ net_edge ],
            PeerNet, NetTx: [ net_edge ],
            // browse: edge -> session -> peer_net -> edge read-model
            Session, PeerBrowseConnect: [ peer_net ],
            Session, BrowseFailed: [ api_server ],
            PeerNet, BrowseListingOwnedView: [ api_server ],
            PeerNet, BrowseFailed: [ api_server ],
            // serving: incoming searches matched + delivered; firewall pierces
            Session, IncomingSearch: [ peer_net ],
            Session, SetExcludedPhrases: [ peer_net ],
            Session, PeerPierce: [ peer_net ],
            Session, PeerPierceFile: [ peer_net ],
            Session, RelayDistribSearch: [ peer_net ],
            Session, DistribSpeedLimits: [ peer_net ],
            Session, PeerPierceDistrib: [ peer_net ],
            // requesting: filtered inbound search results -> edge
            PeerNet, SearchResultReceived: [ api_server ],
            // downloads: edge -> session resolves address -> peer_net -> edge log
            Session, PeerDownloadConnect: [ peer_net ],
            Session, DownloadFailed: [ api_server ],
            PeerNet, DownloadComplete: [ api_server ],
            PeerNet, DownloadFailed: [ api_server ],
            PeerNet, DownloadQueuePosition: [ api_server ],
            // uploads: peer_net asks session to resolve the downloader's address,
            // then opens the file connection and streams.
            PeerNet, ResolveUploadPeer: [ session ],
            Session, PeerUploadConnect: [ peer_net ],
            PeerNet, UploadStarted: [ api_server ],
            PeerNet, TransferProgress: [ api_server ],
            PeerNet, UploadComplete: [ api_server ],
            PeerNet, UploadFailed: [ api_server ],
            // distributed search tree: session adopts a parent -> peer_net D conn
            Session, PeerDistribConnect: [ peer_net ],
            // peer network edge: serving activity -> edge log
            PeerNet, PeerActivity: [ api_server ],
            // broadcasts
            Session, SessionEvent: [ api_server ],
            ConfigStore, ConfigChanged: [ peer_net, updater, session, net_edge, api_server ],
            Updater, UpdaterStatusChanged: [ api_server ],
            // updater background thread -> updater (apply decision)
            Updater, UpdateDownloaded: [ updater ],
        ]
    ExtractWorker:
        handlers: [
            extractor: ExtractorComponent,
        ]
        routes: [
            ApiServer, ExtractRequest: [ extractor ],
            ConfigStore, ConfigChanged: [ extractor ],
        ]
}
