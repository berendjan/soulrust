//! The routing table: which component receives which message from which
//! source, and which worker thread each component lives on.
//!
//! Two workers:
//! - `CoreWorker` hosts everything with fast handlers (session, config,
//!   updater, ui, and both edges' bus-facing halves).
//! - `ExtractWorker` hosts only the extractor, whose Spotify calls can block
//!   for seconds — on its own worker they never stall the core handlers.

use crate::components::browse::Browse;
use crate::components::net_edge::NetEdge;
use crate::components::peer_edge::PeerEdge;
use crate::components::session::Session;
use crate::components::ui::Ui;
use crate::components::updater::Updater;
use crate::components::web_bridge::WebBridge;
use crate::config::ConfigStore;
use crate::extract::ExtractorComponent;
use crate::messages::{
    ApplyUpdateReq, ApplyUpdateResult, BrowseAccepted, BrowseFailed, BrowseHtml, BrowseListing,
    BrowseRenderReq, BrowseUser, ConfigChanged, ConfigSnapshot, ExtractRequest, ExtractResult,
    GetConfigReq, HttpHtml, HttpRender, NetConn, NetRx, NetTx, PeerBrowseConnect, SetConfigReq,
    SetConfigResult, SessionEvent, StartSearch, StartSearchResult, UpdateDownloaded,
    UpdaterStatusChanged,
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
            peer_edge: PeerEdge,
            browse: Browse,
            web_bridge: WebBridge,
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
            // soulseek socket <-> session
            NetEdge, NetConn: [ session ],
            NetEdge, NetRx: [ session ],
            Session, NetTx: [ net_edge ],
            // browse: bridge -> session -> peer edge -> browse read-model
            WebBridge, BrowseUser: [ session ],
            Session, BrowseAccepted: [ web_bridge ],
            Session, PeerBrowseConnect: [ peer_edge ],
            Session, BrowseFailed: [ browse ],
            PeerEdge, BrowseListing: [ browse ],
            PeerEdge, BrowseFailed: [ browse ],
            WebBridge, BrowseRenderReq: [ browse ],
            Browse, BrowseHtml: [ web_bridge ],
            // broadcasts
            Session, SessionEvent: [ ui ],
            ConfigStore, ConfigChanged: [ ui ],
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
