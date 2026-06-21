//! The routing table: which component receives which message from which
//! source, and which worker thread each component lives on.
//!
//! Two workers:
//! - `CoreWorker` hosts everything with fast handlers (session, config,
//!   updater, ui, and both edges' bus-facing halves).
//! - `ExtractWorker` hosts only the extractor, whose Spotify calls can block
//!   for seconds — on its own worker they never stall the core handlers.

use crate::components::api_server::ApiServer;
use crate::components::browse::Browse;
use crate::components::net_edge::NetEdge;
use crate::components::peer_net::PeerNet;
use crate::components::session::Session;
use crate::components::ui::Ui;
use crate::components::updater::Updater;
use crate::components::web_bridge::WebBridge;
use crate::config::ConfigStore;
use crate::extract::ExtractorComponent;
use crate::messages::{
    ApplyUpdateReq, ApplyUpdateResult, BrowseAccepted, BrowseFailed, BrowseHtml, BrowseListing,
    BrowseRenderReq, BrowseUser, CancelDownload, ConfigChanged, ConfigSnapshot, DistribSpeedLimits,
    DownloadComplete,
    DownloadFailed,
    DownloadQueuePosition, ExtractRequest, ExtractResult, GetConfigReq, HttpHtml, HttpRender,
    IncomingSearch, NetConn,
    NetRx, NetTx, PeerActivity, PeerBrowseConnect, PeerDownloadConnect, PeerPierce, PeerPierceDistrib,
    PauseDownload, PeerPierceFile, PeerDistribConnect, PeerUploadConnect, RelayDistribSearch,
    ResolveUploadPeer,
    SearchResultReceived,
    SetConfigReq,
    SetConfigResult, SetExcludedPhrases, SessionEvent, StartDownload, StartSearch, StartSearchResult,
    TransferProgress, UpdateDownloaded,
    UpdaterStatusChanged, UploadComplete, UploadFailed, UploadStarted,
};

rust_messenger::Messenger! {
    crate::config::AppContext,
    CoreWorker:
        handlers: [
            session: Session,
            config_store: ConfigStore,
            updater: Updater,
            ui: Ui,
            net_edge: NetEdge,
            peer_net: PeerNet,
            browse: Browse,
            web_bridge: WebBridge,
            api_server: ApiServer,
        ]
        routes: [
            // http request -> render / act
            WebBridge, HttpRender: [ ui ],
            WebBridge, StartSearch: [ session ],
            WebBridge, GetConfigReq: [ config_store ],
            WebBridge, SetConfigReq: [ config_store ],
            WebBridge, ApplyUpdateReq: [ updater ],
            // responses back to the bridge
            Ui, HttpHtml: [ web_bridge ],
            ExtractorComponent, ExtractResult: [ web_bridge ],
            Session, StartSearchResult: [ web_bridge ],
            ConfigStore, ConfigSnapshot: [ web_bridge ],
            ConfigStore, SetConfigResult: [ web_bridge ],
            Updater, ApplyUpdateResult: [ web_bridge ],
            // soulseek socket <-> session; peer_net also sends server frames
            // (ConnectToPeer relay requests for search delivery).
            NetEdge, NetConn: [ session ],
            NetEdge, NetRx: [ session ],
            Session, NetTx: [ net_edge ],
            PeerNet, NetTx: [ net_edge ],
            // browse: bridge -> session -> peer_net -> browse read-model
            WebBridge, BrowseUser: [ session ],
            Session, BrowseAccepted: [ web_bridge ],
            Session, PeerBrowseConnect: [ peer_net ],
            Session, BrowseFailed: [ browse ],
            PeerNet, BrowseListing: [ browse ],
            PeerNet, BrowseFailed: [ browse ],
            WebBridge, BrowseRenderReq: [ browse ],
            Browse, BrowseHtml: [ web_bridge ],
            // serving: incoming searches matched + delivered; firewall pierces
            Session, IncomingSearch: [ peer_net ],
            Session, SetExcludedPhrases: [ peer_net ],
            Session, PeerPierce: [ peer_net ],
            Session, PeerPierceFile: [ peer_net ],
            Session, RelayDistribSearch: [ peer_net ],
            Session, DistribSpeedLimits: [ peer_net ],
            Session, PeerPierceDistrib: [ peer_net ],
            // requesting: filtered inbound search results -> ui
            PeerNet, SearchResultReceived: [ ui ],
            // downloads: bridge -> session resolves address -> peer_net -> ui log
            WebBridge, StartDownload: [ session, ui ],
            WebBridge, CancelDownload: [ ui, peer_net ],
            WebBridge, PauseDownload: [ ui, peer_net ],
            Session, PeerDownloadConnect: [ peer_net ],
            Session, DownloadFailed: [ ui ],
            PeerNet, DownloadComplete: [ ui ],
            PeerNet, DownloadFailed: [ ui ],
            PeerNet, DownloadQueuePosition: [ ui ],
            // uploads: peer_net asks session to resolve the downloader's address,
            // then opens the file connection and streams.
            PeerNet, ResolveUploadPeer: [ session ],
            Session, PeerUploadConnect: [ peer_net ],
            PeerNet, UploadStarted: [ ui ],
            PeerNet, TransferProgress: [ ui ],
            PeerNet, UploadComplete: [ ui ],
            PeerNet, UploadFailed: [ ui ],
            // distributed search tree: session adopts a parent -> peer_net D conn
            Session, PeerDistribConnect: [ peer_net ],
            // peer network edge: serving activity -> ui log
            PeerNet, PeerActivity: [ ui ],
            // broadcasts
            Session, SessionEvent: [ ui, api_server ],
            ConfigStore, ConfigChanged: [ ui, peer_net, updater, session, net_edge ],
            Updater, UpdaterStatusChanged: [ ui ],
            // updater background thread -> updater (apply decision)
            Updater, UpdateDownloaded: [ updater ],
        ]
    ExtractWorker:
        handlers: [
            extractor: ExtractorComponent,
        ]
        routes: [
            WebBridge, ExtractRequest: [ extractor ],
            ConfigStore, ConfigChanged: [ extractor ],
        ]
}
