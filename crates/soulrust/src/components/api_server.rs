//! The Connect API edge — the app's sole UI-facing surface.
//!
//! It replaces the old htmx `web_bridge` + `ui` + `browse` components: it holds
//! the read-model state (session status, searches + results, downloads/uploads,
//! peer browse listings, activity log), turns Connect RPCs into bus commands,
//! and serves the embedded React SPA — all on one port.
//!
//! Threading model, mirroring the old bridge:
//! - The component itself is a synchronous bus `Handler` on the core worker. Its
//!   `Handle` impls mutate the read-model state and publish fresh snapshots to
//!   per-domain [`tokio_api::sync::watch`] channels, and complete pending
//!   request/reply oneshots by correlation id.
//! - `on_start` spawns a thread running an axum/hyper server on its own tokio
//!   runtime. Async RPC handlers read the latest watch snapshot (or subscribe
//!   for a server-streaming `Watch*`), and send commands onto the bus by pushing
//!   [`BusCommand`]s to a forwarding task that owns a clone of the bus `Writer`.
//!
//! `tokio` here is the proto-deps copy (the one axum/connectrpc are built
//! against), aliased to `tokio_api` in BUILD.bazel so it doesn't collide with
//! the peer reactor's `tokio`.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rust_messenger::traits;
use rust_messenger::traits::extended::Sender;

use axum::body::Body;
use axum::extract::{Path, RawQuery, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response as HttpResponse};
use axum::routing::get;
use connectrpc::{
    ConnectError, RequestContext, Response, Router, ServiceRequest, ServiceResult, ServiceStream,
};

use soulrust_proto::api::soulrust::api::v1 as api;
use soulrust_proto::api_connect::soulrust::api::v1::{
    BrowseService, BrowseServiceExt, ConfigService, ConfigServiceExt, SearchService,
    SearchServiceExt, StatusService, StatusServiceExt, SystemService, SystemServiceExt,
    TransfersService, TransfersServiceExt, UpdaterService, UpdaterServiceExt,
};
use soulrust_proto::MessageField;

use crate::config::{self, AppContext, Config, Control};
use crate::extract::{self, Job};
use crate::messages::{
    ApplyUpdateReq, ApplyUpdateResult, BrowseAccepted, BrowseFailed, BrowseListingOwnedView,
    BrowseUser, CancelDownload, ConfigChanged, DownloadComplete, DownloadFailed,
    DownloadQueuePosition, EnumValue, ExtractRequest, ExtractResult, HandlerId, PauseDownload,
    PeerActivity, RemoveSearch, SearchResultReceived, SessionEvent, SessionEventKind, SetConfigReq,
    StartDownload, StartSearch, StartSearchResult, TransferProgress, UpdaterStatusChanged,
    UpdaterStatusKind, UploadComplete, UploadFailed, UploadStarted,
};
use crate::web_assets_gen::WEB_ASSETS;

/// Default address for the Connect API + web UI.
const DEFAULT_API_ADDR: &str = "127.0.0.1:5031";
const MAX_LOG_LINES: usize = 100;
const MAX_RESULTS_PER_SEARCH: usize = 200;
const MAX_FILES_PER_RESULT: usize = 5000;
const MAX_DOWNLOADS: usize = 200;
/// How long an RPC waits for its bus reply before failing.
const REPLY_TIMEOUT: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------------------
// Internal read-model state (core-worker side)

#[derive(Clone)]
enum SessionStatus {
    Disconnected(String),
    Connecting,
    LoggedIn { greeting: String, own_ip: String },
    LoginFailed(String),
}

struct ResultFile {
    name: String,
    size: u64,
    bitrate: Option<u32>,
    length: Option<u32>,
    vbr: bool,
    sample_rate: Option<u32>,
    bit_depth: Option<u32>,
}

struct SearchResultRow {
    username: String,
    free_slots: bool,
    upload_speed: u32,
    in_queue: u32,
    files: Vec<ResultFile>,
}

struct SearchRow {
    token: u32,
    query: String,
    results: Vec<SearchResultRow>,
    folder: String,
    prefix: String,
    group: String,
    track: u32,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct DownloadEntry {
    username: String,
    filename: String,
    state: DownloadState,
    #[serde(default)]
    bytes: u64,
    #[serde(default)]
    size: u64,
    /// Destination layout the transfer was started with, kept so a paused
    /// download resumes into the same folder under the same name.
    #[serde(default)]
    subdir: String,
    #[serde(default)]
    prefix: String,
}

#[derive(PartialEq, serde::Serialize, serde::Deserialize)]
enum DownloadState {
    Queued,
    Position(u32),
    Starting,
    Completed(String),
    Failed(String),
    Incomplete,
    Paused,
}

impl DownloadState {
    fn is_active(&self) -> bool {
        matches!(
            self,
            DownloadState::Queued | DownloadState::Position(_) | DownloadState::Starting
        )
    }
}

struct UploadEntry {
    username: String,
    filename: String,
    size: u64,
    bytes: u64,
    state: UploadState,
}

enum UploadState {
    Active,
    Completed,
    Failed(String),
}

impl UploadState {
    fn is_active(&self) -> bool {
        matches!(self, UploadState::Active)
    }
}

enum BrowseEntry {
    Loaded(BrowseListingOwnedView),
    Failed(String),
}

// ---------------------------------------------------------------------------
// Cross-thread plumbing shared with the async server

/// A reply awaited by a round-trip RPC, delivered by the matching bus handler.
enum BridgeReply {
    Extract(Result<Job, String>),
    Search {
        started: Vec<api::StartedSearch>,
        error: Option<String>,
    },
    SetConfig(Result<(), String>),
    Apply(Result<(), String>),
    Browse(Option<String>),
}

/// A command an async RPC handler asks the forwarding task to put on the bus.
enum BusCommand {
    Extract {
        corr: u64,
        input: String,
    },
    StartSearch {
        corr: u64,
        source_label: String,
        jobs: Vec<soulrust_proto::bus::SearchJob>,
    },
    RemoveSearch {
        token: u32,
    },
    StartDownload {
        username: String,
        filename: String,
        size: u64,
        subdir: String,
        prefix: String,
    },
    CancelDownload {
        username: String,
        filename: String,
    },
    PauseDownload {
        username: String,
        filename: String,
    },
    BrowseUser {
        corr: u64,
        username: String,
    },
    SetConfig {
        corr: u64,
        config: soulrust_proto::bus::Config,
    },
    ApplyUpdate {
        corr: u64,
    },
}

type Pending = Mutex<HashMap<u64, tokio_api::sync::oneshot::Sender<BridgeReply>>>;

/// State shared between the core-worker component and the async server.
struct Shared {
    cmd_tx: tokio_api::sync::mpsc::UnboundedSender<BusCommand>,
    pending: Pending,
    corr: AtomicU64,
    control: Arc<Control>,
    config_path: PathBuf,
    /// The full current config, including secrets never sent over the wire
    /// (password, client_secret, refresh_token). The source of truth for merging
    /// SetConfig (empty secret = keep) and for the Spotify OAuth flow.
    current: Mutex<Config>,
    /// Pending Spotify OAuth `state` nonce between /spotify/login and its callback.
    oauth_state: Mutex<Option<String>>,
    status_tx: tokio_api::sync::watch::Sender<api::Status>,
    searches_tx: tokio_api::sync::watch::Sender<api::Searches>,
    transfers_tx: tokio_api::sync::watch::Sender<api::Transfers>,
    browse_tx: tokio_api::sync::watch::Sender<api::BrowseListings>,
    config_tx: tokio_api::sync::watch::Sender<api::Config>,
    updater_tx: tokio_api::sync::watch::Sender<api::UpdaterStatus>,
}

impl Shared {
    /// Register a reply channel, send the command, and await the reply.
    async fn round_trip(
        &self,
        make: impl FnOnce(u64) -> BusCommand,
    ) -> Result<BridgeReply, ConnectError> {
        let corr = self.corr.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = tokio_api::sync::oneshot::channel();
        self.pending.lock().unwrap().insert(corr, tx);
        if self.cmd_tx.send(make(corr)).is_err() {
            self.pending.lock().unwrap().remove(&corr);
            return Err(ConnectError::unavailable("api server is shutting down"));
        }
        match tokio_api::time::timeout(REPLY_TIMEOUT, rx).await {
            Ok(Ok(reply)) => Ok(reply),
            _ => {
                self.pending.lock().unwrap().remove(&corr);
                Err(ConnectError::unavailable(
                    "timed out waiting for the app to respond",
                ))
            }
        }
    }

    fn send(&self, cmd: BusCommand) {
        let _ = self.cmd_tx.send(cmd);
    }
}

// ---------------------------------------------------------------------------
// The bus component

pub struct ApiServer {
    addr: String,
    open_browser: bool,
    shared: Arc<Shared>,
    cmd_rx: Option<tokio_api::sync::mpsc::UnboundedReceiver<BusCommand>>,
    // read-model state (only the core worker touches these)
    session: SessionStatus,
    searches: Vec<SearchRow>,
    downloads: Vec<DownloadEntry>,
    downloads_path: PathBuf,
    uploads: Vec<UploadEntry>,
    browse_order: Vec<String>,
    browse_entries: HashMap<String, BrowseEntry>,
    log: VecDeque<String>,
    username: String,
}

impl ApiServer {
    pub fn new<W: traits::core::Writer>(ctx: &AppContext, _writer: &W) -> Self {
        let (cmd_tx, cmd_rx) = tokio_api::sync::mpsc::unbounded_channel();

        // Reconstruct the downloads list from the persisted history + a disk scan
        // (same behaviour the old Ui component had).
        let downloads_path = ctx.config_path.with_file_name("soulrust-downloads.json");
        let mut downloads = load_downloads(&downloads_path);
        let seen: std::collections::HashSet<String> = downloads
            .iter()
            .map(|d| basename(&d.filename).to_owned())
            .collect();
        for entry in scan_disk_downloads(
            &ctx.config.sharing.download_path(),
            &ctx.config.sharing.incomplete_path(),
        ) {
            if !seen.contains(basename(&entry.filename)) {
                downloads.push(entry);
            }
        }
        downloads.truncate(MAX_DOWNLOADS);

        let username = ctx.config.server.username.clone();
        let (status_tx, _) = tokio_api::sync::watch::channel(api::Status {
            username: username.clone(),
            connection: api::ConnectionState::ConnectionDisconnected.into(),
            detail: "starting up".into(),
            ..Default::default()
        });
        let (searches_tx, _) = tokio_api::sync::watch::channel(api::Searches::default());
        let (transfers_tx, _) =
            tokio_api::sync::watch::channel(transfers_snapshot(&downloads, &[]));
        let (browse_tx, _) = tokio_api::sync::watch::channel(api::BrowseListings::default());
        let (config_tx, _) = tokio_api::sync::watch::channel(config_to_api(&ctx.config));
        let (updater_tx, _) = tokio_api::sync::watch::channel(api::UpdaterStatus::default());

        let shared = Arc::new(Shared {
            cmd_tx,
            pending: Mutex::new(HashMap::new()),
            corr: AtomicU64::new(0),
            control: ctx.control.clone(),
            config_path: ctx.config_path.clone(),
            current: Mutex::new(ctx.config.clone()),
            oauth_state: Mutex::new(None),
            status_tx,
            searches_tx,
            transfers_tx,
            browse_tx,
            config_tx,
            updater_tx,
        });

        // Serve on the configured UI address; fall back to the default if it is
        // unset or unparseable.
        let addr = {
            let a = ctx.config.ui.bind_addr.trim();
            if a.parse::<SocketAddr>().is_ok() {
                a.to_owned()
            } else {
                DEFAULT_API_ADDR.to_owned()
            }
        };
        ApiServer {
            addr,
            open_browser: ctx.config.ui.open_browser,
            shared,
            cmd_rx: Some(cmd_rx),
            session: SessionStatus::Disconnected("starting up".into()),
            searches: Vec::new(),
            downloads,
            downloads_path,
            uploads: Vec::new(),
            browse_order: Vec::new(),
            browse_entries: HashMap::new(),
            log: VecDeque::new(),
            username,
        }
    }

    fn complete(&self, corr: u64, reply: BridgeReply) {
        if let Some(tx) = self.shared.pending.lock().unwrap().remove(&corr) {
            let _ = tx.send(reply);
        }
    }

    fn log(&mut self, line: String) {
        if self.log.len() >= MAX_LOG_LINES {
            self.log.pop_front();
        }
        self.log.push_back(line);
        self.publish_status();
    }

    // --- snapshot publishers ------------------------------------------------

    fn publish_status(&self) {
        let (connection, detail, greeting, own_ip) = match &self.session {
            SessionStatus::Disconnected(r) => (
                api::ConnectionState::ConnectionDisconnected,
                r.clone(),
                String::new(),
                String::new(),
            ),
            SessionStatus::Connecting => (
                api::ConnectionState::ConnectionConnecting,
                String::new(),
                String::new(),
                String::new(),
            ),
            SessionStatus::LoggedIn { greeting, own_ip } => (
                api::ConnectionState::ConnectionLoggedIn,
                String::new(),
                greeting.clone(),
                own_ip.clone(),
            ),
            SessionStatus::LoginFailed(r) => (
                api::ConnectionState::ConnectionLoginFailed,
                r.clone(),
                String::new(),
                String::new(),
            ),
        };
        let _ = self.shared.status_tx.send_replace(api::Status {
            logged_in: matches!(self.session, SessionStatus::LoggedIn { .. }),
            username: self.username.clone(),
            greeting,
            own_ip,
            shared_files: 0,
            connection: connection.into(),
            detail,
            log: self.log.iter().cloned().collect(),
            ..Default::default()
        });
    }

    fn publish_searches(&self) {
        let searches = self
            .searches
            .iter()
            .map(|s| api::Search {
                token: s.token,
                query: s.query.clone(),
                folder: s.folder.clone(),
                prefix: s.prefix.clone(),
                group: s.group.clone(),
                track: s.track,
                results: s
                    .results
                    .iter()
                    .map(|r| api::SearchResult {
                        username: r.username.clone(),
                        free_slots: r.free_slots,
                        upload_speed: r.upload_speed,
                        in_queue: r.in_queue,
                        files: r
                            .files
                            .iter()
                            .map(|f| api::ResultFile {
                                name: f.name.clone(),
                                size: f.size,
                                bitrate: f.bitrate.unwrap_or(0),
                                length: f.length.unwrap_or(0),
                                vbr: f.vbr,
                                sample_rate: f.sample_rate.unwrap_or(0),
                                bit_depth: f.bit_depth.unwrap_or(0),
                                ..Default::default()
                            })
                            .collect(),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            })
            .collect();
        let _ = self.shared.searches_tx.send_replace(api::Searches {
            searches,
            ..Default::default()
        });
    }

    fn publish_transfers(&self) {
        let _ = self
            .shared
            .transfers_tx
            .send_replace(transfers_snapshot(&self.downloads, &self.uploads));
    }

    fn publish_browse(&self) {
        let users = self
            .browse_order
            .iter()
            .filter_map(|username| {
                let entry = self.browse_entries.get(username)?;
                Some(match entry {
                    BrowseEntry::Failed(reason) => api::BrowseUserListing {
                        username: username.clone(),
                        error: reason.clone(),
                        ..Default::default()
                    },
                    BrowseEntry::Loaded(listing) => browse_listing_to_api(listing),
                })
            })
            .collect();
        let _ = self.shared.browse_tx.send_replace(api::BrowseListings {
            users,
            ..Default::default()
        });
    }

    // --- download/upload state helpers (ported from the old Ui) -------------

    fn save_downloads(&self) {
        let history: Vec<&DownloadEntry> = self
            .downloads
            .iter()
            .filter(|d| !d.state.is_active())
            .collect();
        if let Ok(json) = serde_json::to_string(&history) {
            let _ = std::fs::write(&self.downloads_path, json);
        }
    }

    fn set_download_state(&mut self, username: &str, filename: &str, state: DownloadState) {
        if let Some(d) = self
            .downloads
            .iter_mut()
            .find(|d| d.username == username && d.filename == filename)
        {
            d.state = state;
            return;
        }
        self.downloads.push(DownloadEntry {
            username: username.to_owned(),
            filename: filename.to_owned(),
            state,
            bytes: 0,
            size: 0,
            subdir: String::new(),
            prefix: String::new(),
        });
        if self.downloads.len() > MAX_DOWNLOADS {
            let evict = self
                .downloads
                .iter()
                .position(|d| !d.state.is_active())
                .unwrap_or(0);
            self.downloads.remove(evict);
        }
    }

    fn set_upload_state(&mut self, username: &str, filename: &str, state: UploadState) {
        if let Some(u) = self
            .uploads
            .iter_mut()
            .find(|u| u.username == username && u.filename == filename && u.state.is_active())
        {
            u.state = state;
        }
    }

    fn touch_browse(&mut self, username: &str) {
        self.browse_order.retain(|u| u != username);
        self.browse_order.insert(0, username.to_owned());
    }
}

impl traits::core::Handler for ApiServer {
    type Id = HandlerId;
    const ID: HandlerId = HandlerId::ApiServer;

    fn on_start<W: traits::core::Writer>(&mut self, writer: &W) {
        let addr = self.addr.clone();
        let open_browser = self.open_browser;
        let shared = self.shared.clone();
        let cmd_rx = self.cmd_rx.take().expect("on_start called once");
        let writer = writer.clone();
        std::thread::Builder::new()
            .name("soulrust-api".into())
            .spawn(move || serve(addr, open_browser, shared, cmd_rx, writer))
            .expect("spawning api-server thread");
        self.publish_status();
    }
}

// --- bus event handlers: mutate state, publish snapshots, complete replies --

impl traits::core::Handle<SessionEvent> for ApiServer {
    fn handle<W: traits::core::Writer>(&mut self, message: &SessionEvent, _writer: &W) {
        match message.kind {
            EnumValue::Known(SessionEventKind::SessionConnecting) => {
                self.session = SessionStatus::Connecting;
                self.publish_status();
            }
            EnumValue::Known(SessionEventKind::SessionLoggedIn) => {
                self.session = SessionStatus::LoggedIn {
                    greeting: message.greeting.clone(),
                    own_ip: message.own_ip.clone(),
                };
                self.publish_status();
            }
            EnumValue::Known(SessionEventKind::SessionLoginFailed) => {
                self.session = SessionStatus::LoginFailed(message.reason.clone());
                self.publish_status();
            }
            EnumValue::Known(SessionEventKind::SessionDisconnected) => {
                self.session = SessionStatus::Disconnected(message.reason.clone());
                self.publish_status();
            }
            EnumValue::Known(SessionEventKind::SessionSearchStarted) => {
                self.searches.push(SearchRow {
                    token: message.token,
                    query: message.query.clone(),
                    results: Vec::new(),
                    folder: message.folder.clone(),
                    prefix: message.prefix.clone(),
                    group: message.group.clone(),
                    track: message.track,
                });
                self.publish_searches();
            }
            EnumValue::Known(SessionEventKind::SessionSearchBroadcastSeen) => {
                self.log(format!(
                    "search on the network: {}: {}",
                    message.username, message.query
                ));
            }
            EnumValue::Known(SessionEventKind::SessionProtocolNote) => {
                self.log(message.note.clone())
            }
            _ => {}
        }
    }
}

impl traits::core::Handle<ConfigChanged> for ApiServer {
    fn handle<W: traits::core::Writer>(&mut self, message: &ConfigChanged, _writer: &W) {
        let config = config::config_from_proto(&message.config);
        self.username = config.server.username.clone();
        *self.shared.current.lock().unwrap() = config.clone();
        let _ = self.shared.config_tx.send_replace(config_to_api(&config));
        self.log("configuration updated".into());
    }
}

impl traits::core::Handle<UpdaterStatusChanged> for ApiServer {
    fn handle<W: traits::core::Writer>(&mut self, message: &UpdaterStatusChanged, _writer: &W) {
        let _ = self.shared.updater_tx.send_replace(updater_to_api(message));
    }
}

impl traits::core::Handle<PeerActivity> for ApiServer {
    fn handle<W: traits::core::Writer>(&mut self, message: &PeerActivity, _writer: &W) {
        self.log(message.note.clone());
    }
}

impl traits::core::Handle<StartDownload> for ApiServer {
    fn handle<W: traits::core::Writer>(&mut self, message: &StartDownload, _writer: &W) {
        self.set_download_state(&message.username, &message.filename, DownloadState::Queued);
        // Remember where this transfer is headed so a later Resume can ask for
        // the same destination.
        if let Some(d) = self
            .downloads
            .iter_mut()
            .find(|d| d.username == message.username && d.filename == message.filename)
        {
            d.size = message.size;
            d.subdir = message.subdir.clone();
            d.prefix = message.prefix.clone();
        }
        self.publish_transfers();
    }
}

impl traits::core::Handle<CancelDownload> for ApiServer {
    fn handle<W: traits::core::Writer>(&mut self, message: &CancelDownload, _writer: &W) {
        self.downloads
            .retain(|d| !(d.username == message.username && d.filename == message.filename));
        self.save_downloads();
        self.publish_transfers();
    }
}

impl traits::core::Handle<PauseDownload> for ApiServer {
    fn handle<W: traits::core::Writer>(&mut self, message: &PauseDownload, _writer: &W) {
        self.set_download_state(&message.username, &message.filename, DownloadState::Paused);
        self.save_downloads();
        self.publish_transfers();
    }
}

impl traits::core::Handle<DownloadComplete> for ApiServer {
    fn handle<W: traits::core::Writer>(&mut self, message: &DownloadComplete, _writer: &W) {
        self.set_download_state(
            &message.username,
            &message.filename,
            DownloadState::Completed(message.path.clone()),
        );
        self.save_downloads();
        self.publish_transfers();
        self.log(format!(
            "downloaded {} from {} → {}",
            message.filename, message.username, message.path
        ));
    }
}

impl traits::core::Handle<DownloadFailed> for ApiServer {
    fn handle<W: traits::core::Writer>(&mut self, message: &DownloadFailed, _writer: &W) {
        self.set_download_state(
            &message.username,
            &message.filename,
            DownloadState::Failed(message.reason.clone()),
        );
        self.save_downloads();
        self.publish_transfers();
        self.log(format!(
            "download of {} from {} failed: {}",
            message.filename, message.username, message.reason
        ));
    }
}

impl traits::core::Handle<DownloadQueuePosition> for ApiServer {
    fn handle<W: traits::core::Writer>(&mut self, message: &DownloadQueuePosition, _writer: &W) {
        let updatable = self
            .downloads
            .iter()
            .find(|d| d.username == message.username && d.filename == message.filename)
            .is_none_or(|d| d.state.is_active());
        if updatable {
            let state = if message.place == 0 {
                DownloadState::Starting
            } else {
                DownloadState::Position(message.place)
            };
            self.set_download_state(&message.username, &message.filename, state);
            self.publish_transfers();
        }
    }
}

impl traits::core::Handle<TransferProgress> for ApiServer {
    fn handle<W: traits::core::Writer>(&mut self, message: &TransferProgress, _writer: &W) {
        if message.upload {
            if let Some(u) = self.uploads.iter_mut().find(|u| {
                u.state.is_active()
                    && u.username == message.username
                    && u.filename == message.filename
            }) {
                u.bytes = message.bytes;
                if message.size > 0 {
                    u.size = message.size;
                }
            }
        } else if let Some(d) = self.downloads.iter_mut().find(|d| {
            d.state.is_active() && d.username == message.username && d.filename == message.filename
        }) {
            d.bytes = message.bytes;
            d.size = message.size;
        }
        self.publish_transfers();
    }
}

impl traits::core::Handle<UploadStarted> for ApiServer {
    fn handle<W: traits::core::Writer>(&mut self, message: &UploadStarted, _writer: &W) {
        if self.uploads.len() >= MAX_DOWNLOADS {
            self.uploads.remove(0);
        }
        self.uploads.push(UploadEntry {
            username: message.username.clone(),
            filename: message.filename.clone(),
            size: message.size,
            bytes: 0,
            state: UploadState::Active,
        });
        self.publish_transfers();
    }
}

impl traits::core::Handle<UploadComplete> for ApiServer {
    fn handle<W: traits::core::Writer>(&mut self, message: &UploadComplete, _writer: &W) {
        self.set_upload_state(&message.username, &message.filename, UploadState::Completed);
        self.publish_transfers();
        self.log(format!(
            "uploaded {} to {}",
            message.filename, message.username
        ));
    }
}

impl traits::core::Handle<UploadFailed> for ApiServer {
    fn handle<W: traits::core::Writer>(&mut self, message: &UploadFailed, _writer: &W) {
        self.set_upload_state(
            &message.username,
            &message.filename,
            UploadState::Failed(message.reason.clone()),
        );
        self.publish_transfers();
        self.log(format!(
            "upload of {} to {} failed: {}",
            message.filename, message.username, message.reason
        ));
    }
}

impl traits::core::Handle<SearchResultReceived> for ApiServer {
    fn handle<W: traits::core::Writer>(&mut self, message: &SearchResultReceived, _writer: &W) {
        let Some(search) = self.searches.iter_mut().find(|s| s.token == message.token) else {
            return;
        };
        let mut incoming = message.files.iter().map(|f| ResultFile {
            name: f.name.clone(),
            size: f.size,
            bitrate: f.bitrate,
            length: f.length,
            vbr: f.vbr,
            sample_rate: f.sample_rate,
            bit_depth: f.bit_depth,
        });
        if let Some(existing) = search
            .results
            .iter_mut()
            .find(|r| r.username == message.username)
        {
            let room = MAX_FILES_PER_RESULT.saturating_sub(existing.files.len());
            existing.files.extend(incoming.by_ref().take(room));
            existing.free_slots = message.free_slots;
            existing.upload_speed = message.upload_speed;
            existing.in_queue = message.in_queue;
        } else if search.results.len() < MAX_RESULTS_PER_SEARCH {
            search.results.push(SearchResultRow {
                username: message.username.clone(),
                free_slots: message.free_slots,
                upload_speed: message.upload_speed,
                in_queue: message.in_queue,
                files: incoming.take(MAX_FILES_PER_RESULT).collect(),
            });
        }
        self.publish_searches();
    }
}

impl traits::core::Handle<RemoveSearch> for ApiServer {
    fn handle<W: traits::core::Writer>(&mut self, message: &RemoveSearch, _writer: &W) {
        self.searches.retain(|s| s.token != message.token);
        self.publish_searches();
    }
}

impl traits::core::Handle<BrowseListingOwnedView> for ApiServer {
    fn handle<W: traits::core::Writer>(&mut self, message: &BrowseListingOwnedView, _writer: &W) {
        let username = message.view().username.to_owned();
        self.touch_browse(&username);
        self.browse_entries
            .insert(username, BrowseEntry::Loaded(message.clone()));
        self.publish_browse();
    }
}

impl traits::core::Handle<BrowseFailed> for ApiServer {
    fn handle<W: traits::core::Writer>(&mut self, message: &BrowseFailed, _writer: &W) {
        self.touch_browse(&message.username);
        self.browse_entries.insert(
            message.username.clone(),
            BrowseEntry::Failed(message.reason.clone()),
        );
        self.publish_browse();
    }
}

// response messages: complete the awaiting RPC

impl traits::core::Handle<ExtractResult> for ApiServer {
    fn handle<W: traits::core::Writer>(&mut self, message: &ExtractResult, _writer: &W) {
        let result = match &message.error {
            Some(err) => Err(err.clone()),
            None => Ok(extract::job_from_proto(&message.job)),
        };
        self.complete(message.corr, BridgeReply::Extract(result));
    }
}

impl traits::core::Handle<StartSearchResult> for ApiServer {
    fn handle<W: traits::core::Writer>(&mut self, message: &StartSearchResult, _writer: &W) {
        let started = message
            .started
            .iter()
            .map(|s| api::StartedSearch {
                token: s.token,
                query: s.query.clone(),
                ..Default::default()
            })
            .collect();
        self.complete(
            message.corr,
            BridgeReply::Search {
                started,
                error: message.error.clone(),
            },
        );
    }
}

impl traits::core::Handle<crate::messages::SetConfigResult> for ApiServer {
    fn handle<W: traits::core::Writer>(
        &mut self,
        message: &crate::messages::SetConfigResult,
        _writer: &W,
    ) {
        self.complete(
            message.corr,
            BridgeReply::SetConfig(message.error.clone().map_or(Ok(()), Err)),
        );
    }
}

impl traits::core::Handle<ApplyUpdateResult> for ApiServer {
    fn handle<W: traits::core::Writer>(&mut self, message: &ApplyUpdateResult, _writer: &W) {
        self.complete(
            message.corr,
            BridgeReply::Apply(message.error.clone().map_or(Ok(()), Err)),
        );
    }
}

impl traits::core::Handle<BrowseAccepted> for ApiServer {
    fn handle<W: traits::core::Writer>(&mut self, message: &BrowseAccepted, _writer: &W) {
        self.complete(message.corr, BridgeReply::Browse(message.error.clone()));
    }
}

// ---------------------------------------------------------------------------
// The async server

fn serve<W: traits::core::Writer + Clone + Send + 'static>(
    addr: String,
    open_browser: bool,
    shared: Arc<Shared>,
    cmd_rx: tokio_api::sync::mpsc::UnboundedReceiver<BusCommand>,
    writer: W,
) {
    let runtime = match tokio_api::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("api server: cannot start runtime: {err}");
            return;
        }
    };
    runtime.block_on(async move {
        // Forwarding task: drains commands and puts them on the bus. Owns the
        // Writer clone, so the async handlers never touch `W` directly.
        tokio_api::spawn(forward_commands(cmd_rx, writer));

        let api = Arc::new(Api {
            shared: shared.clone(),
        });
        let mut router = Router::new();
        router = StatusServiceExt::register(api.clone(), router);
        router = SearchServiceExt::register(api.clone(), router);
        router = TransfersServiceExt::register(api.clone(), router);
        router = BrowseServiceExt::register(api.clone(), router);
        router = ConfigServiceExt::register(api.clone(), router);
        router = UpdaterServiceExt::register(api.clone(), router);
        router = SystemServiceExt::register(api.clone(), router);

        let app = axum::Router::new()
            .route("/", get(serve_index))
            .route("/assets/{*path}", get(serve_asset))
            .route("/media", get(serve_media))
            .route("/spotify/login", get(spotify_login))
            .route("/spotify/callback", get(spotify_callback))
            .with_state(shared.clone())
            // The Connect service (POST /soulrust.api.v1.*) is the fallback; it
            // never collides with the GET routes above.
            //
            // No CORS layer: the SPA is served same-origin from this port and the
            // dev server reaches the API through Vite's proxy, so no cross-origin
            // access is needed. Omitting it stops other origins (the browser
            // blocks cross-origin reads without CORS headers) from reaching this
            // unauthenticated loopback API.
            .fallback_service(router.into_axum_service());

        let listener = match bind_with_retry(&addr).await {
            Ok(listener) => listener,
            Err(err) => {
                eprintln!(
                    "soulrust: cannot bind {addr}: {err}\n\
                     Another soulrust may already be running on that port — quit it \
                     (or change ui.bind_addr) and start again."
                );
                // Without the UI/API there's nothing to control, so exit cleanly
                // rather than linger as a dead process holding the bus.
                shared.control.quit.store(true, Ordering::SeqCst);
                return;
            }
        };
        println!("soulrust Connect API + web UI listening on http://{addr}");
        if open_browser {
            os_open(&format!("http://{addr}"));
        }
        if let Err(err) = axum::serve(listener, app).await {
            eprintln!("api server: stopped: {err}");
        }
    });
}

/// Bind the API port, retrying briefly before giving up.
///
/// Restart on non-unix spawns the replacement process and only then lets the
/// old one exit, so for a moment both hold the port. A short retry rides that
/// out; a genuine conflict still fails, loudly, within a few seconds.
async fn bind_with_retry(addr: &str) -> std::io::Result<tokio_api::net::TcpListener> {
    const ATTEMPTS: u32 = 25;
    const INTERVAL: Duration = Duration::from_millis(200);
    for _ in 1..ATTEMPTS {
        match tokio_api::net::TcpListener::bind(addr).await {
            Ok(listener) => return Ok(listener),
            Err(_) => tokio_api::time::sleep(INTERVAL).await,
        }
    }
    tokio_api::net::TcpListener::bind(addr).await
}

async fn forward_commands<W: traits::core::Writer>(
    mut rx: tokio_api::sync::mpsc::UnboundedReceiver<BusCommand>,
    writer: W,
) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            BusCommand::Extract { corr, input } => {
                ApiServer::send(
                    &ExtractRequest {
                        corr,
                        input,
                        ..Default::default()
                    },
                    &writer,
                );
            }
            BusCommand::StartSearch {
                corr,
                source_label,
                jobs,
            } => {
                ApiServer::send(
                    &StartSearch {
                        corr,
                        source_label,
                        jobs,
                        ..Default::default()
                    },
                    &writer,
                );
            }
            BusCommand::RemoveSearch { token } => {
                ApiServer::send(
                    &RemoveSearch {
                        token,
                        ..Default::default()
                    },
                    &writer,
                );
            }
            BusCommand::StartDownload {
                username,
                filename,
                size,
                subdir,
                prefix,
            } => {
                ApiServer::send(
                    &StartDownload {
                        username,
                        filename,
                        size,
                        subdir,
                        prefix,
                        ..Default::default()
                    },
                    &writer,
                );
            }
            BusCommand::CancelDownload { username, filename } => {
                ApiServer::send(
                    &CancelDownload {
                        username,
                        filename,
                        ..Default::default()
                    },
                    &writer,
                );
            }
            BusCommand::PauseDownload { username, filename } => {
                ApiServer::send(
                    &PauseDownload {
                        username,
                        filename,
                        ..Default::default()
                    },
                    &writer,
                );
            }
            BusCommand::BrowseUser { corr, username } => {
                ApiServer::send(
                    &BrowseUser {
                        corr,
                        username,
                        ..Default::default()
                    },
                    &writer,
                );
            }
            BusCommand::SetConfig { corr, config } => {
                ApiServer::send(
                    &SetConfigReq {
                        corr,
                        config: MessageField::some(config),
                        ..Default::default()
                    },
                    &writer,
                );
            }
            BusCommand::ApplyUpdate { corr } => {
                ApiServer::send(
                    &ApplyUpdateReq {
                        corr,
                        ..Default::default()
                    },
                    &writer,
                );
            }
        }
    }
}

/// The connect service implementation — one struct implementing every service
/// trait; all handlers read from / send through the shared hub.
struct Api {
    shared: Arc<Shared>,
}

/// A server-streaming body over a watch channel: emits the current snapshot,
/// then a fresh one on every change, ending when the sender is dropped.
fn watch_stream<T: Clone + Send + Sync + 'static>(
    rx: tokio_api::sync::watch::Receiver<T>,
) -> impl futures::Stream<Item = Result<T, ConnectError>> + Send + 'static {
    futures::stream::unfold((rx, true), |(mut rx, first)| async move {
        if first {
            let value = rx.borrow_and_update().clone();
            return Some((Ok(value), (rx, false)));
        }
        match rx.changed().await {
            Ok(()) => {
                let value = rx.borrow_and_update().clone();
                Some((Ok(value), (rx, false)))
            }
            Err(_) => None,
        }
    })
}

impl StatusService for Api {
    #[allow(refining_impl_trait)]
    async fn get_status(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, api::Empty>,
    ) -> ServiceResult<api::Status> {
        Response::ok(self.shared.status_tx.borrow().clone())
    }

    #[allow(refining_impl_trait)]
    async fn watch_status(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, api::Empty>,
    ) -> ServiceResult<ServiceStream<api::Status>> {
        Response::stream_ok(watch_stream(self.shared.status_tx.subscribe()))
    }
}

impl SearchService for Api {
    #[allow(refining_impl_trait)]
    async fn search(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, api::SearchRequest>,
    ) -> ServiceResult<api::SearchResponse> {
        let req = request.to_owned_message();
        // 1. Extract the input into jobs.
        let job = match self
            .shared
            .round_trip(|corr| BusCommand::Extract {
                corr,
                input: req.input.clone(),
            })
            .await?
        {
            BridgeReply::Extract(Ok(job)) => job,
            BridgeReply::Extract(Err(error)) => {
                return Response::ok(api::SearchResponse {
                    error,
                    ..Default::default()
                });
            }
            _ => return Err(ConnectError::internal("unexpected reply")),
        };
        // 2. Optionally organize downloads into a numbered subfolder. This is a
        //    persistent preference (config.sharing.organize_downloads), applied
        //    to any playlist/album source.
        let organize = self
            .shared
            .current
            .lock()
            .unwrap()
            .sharing
            .organize_downloads;
        let subdir = organize
            .then(|| {
                job.folder
                    .as_deref()
                    .map(crate::components::sanitize_path_component)
            })
            .flatten()
            .filter(|s| !s.is_empty());
        // Re-running one track of a playlist (the card's edit pencil) sends a
        // bare query, which extracts to a single job with no collection around
        // it. Carry the replaced card's placement over so the track stays in its
        // group card and still downloads into the numbered playlist folder.
        let inherited = (req.replace_token > 0)
            .then(|| placement_of(&self.shared.searches_tx.borrow(), req.replace_token))
            .flatten();
        let jobs = build_jobs(&job, subdir.as_deref(), inherited.as_ref());
        let source_label = job.source_label.clone();
        // 3. Start the searches.
        let (started, error) = match self
            .shared
            .round_trip(move |corr| BusCommand::StartSearch {
                corr,
                source_label,
                jobs,
            })
            .await?
        {
            BridgeReply::Search { started, error } => (started, error),
            _ => return Err(ConnectError::internal("unexpected reply")),
        };
        if error.is_none() && req.replace_token > 0 {
            self.shared.send(BusCommand::RemoveSearch {
                token: req.replace_token,
            });
        }
        Response::ok(api::SearchResponse {
            started,
            error: error.unwrap_or_default(),
            ..Default::default()
        })
    }

    #[allow(refining_impl_trait)]
    async fn remove_search(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, api::RemoveSearchRequest>,
    ) -> ServiceResult<api::Empty> {
        let req = request.to_owned_message();
        self.shared
            .send(BusCommand::RemoveSearch { token: req.token });
        Response::ok(api::Empty::default())
    }

    #[allow(refining_impl_trait)]
    async fn watch_searches(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, api::Empty>,
    ) -> ServiceResult<ServiceStream<api::Searches>> {
        Response::stream_ok(watch_stream(self.shared.searches_tx.subscribe()))
    }
}

impl TransfersService for Api {
    #[allow(refining_impl_trait)]
    async fn start_download(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, api::StartDownloadRequest>,
    ) -> ServiceResult<api::Empty> {
        let req = request.to_owned_message();
        if req.username.is_empty() || req.filename.is_empty() {
            return Err(ConnectError::invalid_argument(
                "username and filename are required",
            ));
        }
        self.shared.send(BusCommand::StartDownload {
            username: req.username,
            filename: req.filename,
            size: req.size,
            subdir: req.subdir,
            prefix: req.prefix,
        });
        Response::ok(api::Empty::default())
    }

    #[allow(refining_impl_trait)]
    async fn cancel_download(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, api::TransferRef>,
    ) -> ServiceResult<api::Empty> {
        let req = request.to_owned_message();
        self.shared.send(BusCommand::CancelDownload {
            username: req.username,
            filename: req.filename,
        });
        Response::ok(api::Empty::default())
    }

    #[allow(refining_impl_trait)]
    async fn pause_download(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, api::TransferRef>,
    ) -> ServiceResult<api::Empty> {
        let req = request.to_owned_message();
        self.shared.send(BusCommand::PauseDownload {
            username: req.username,
            filename: req.filename,
        });
        Response::ok(api::Empty::default())
    }

    #[allow(refining_impl_trait)]
    async fn watch_transfers(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, api::Empty>,
    ) -> ServiceResult<ServiceStream<api::Transfers>> {
        Response::stream_ok(watch_stream(self.shared.transfers_tx.subscribe()))
    }
}

impl BrowseService for Api {
    #[allow(refining_impl_trait)]
    async fn browse(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, api::BrowseRequest>,
    ) -> ServiceResult<api::BrowseResponse> {
        let req = request.to_owned_message();
        let error = match self
            .shared
            .round_trip(|corr| BusCommand::BrowseUser {
                corr,
                username: req.username.clone(),
            })
            .await?
        {
            BridgeReply::Browse(error) => error,
            _ => return Err(ConnectError::internal("unexpected reply")),
        };
        Response::ok(api::BrowseResponse {
            error: error.unwrap_or_default(),
            ..Default::default()
        })
    }

    #[allow(refining_impl_trait)]
    async fn watch_browse(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, api::Empty>,
    ) -> ServiceResult<ServiceStream<api::BrowseListings>> {
        Response::stream_ok(watch_stream(self.shared.browse_tx.subscribe()))
    }
}

impl ConfigService for Api {
    #[allow(refining_impl_trait)]
    async fn get_config(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, api::Empty>,
    ) -> ServiceResult<api::Config> {
        Response::ok(self.shared.config_tx.borrow().clone())
    }

    #[allow(refining_impl_trait)]
    async fn set_config(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, api::Config>,
    ) -> ServiceResult<api::SetConfigResponse> {
        let incoming = request.to_owned_message();
        if let Err(error) = validate_config(&incoming) {
            return Response::ok(api::SetConfigResponse {
                error,
                ..Default::default()
            });
        }
        let mut serde = api_config_to_serde(&incoming);
        // Merge secrets from the stored config: the client never receives them,
        // so an empty field means "keep". refresh_token is server-managed (only
        // the OAuth callback writes it), so it is always preserved here.
        {
            let current = self.shared.current.lock().unwrap();
            if serde.server.password.is_empty() {
                serde.server.password = current.server.password.clone();
            }
            if serde
                .spotify
                .client_secret
                .as_deref()
                .is_none_or(str::is_empty)
            {
                serde.spotify.client_secret = current.spotify.client_secret.clone();
            }
            serde.spotify.refresh_token = current.spotify.refresh_token.clone();
        }
        let cfg = config::config_to_proto(&serde);
        let result = match self
            .shared
            .round_trip(|corr| BusCommand::SetConfig { corr, config: cfg })
            .await?
        {
            BridgeReply::SetConfig(result) => result,
            _ => return Err(ConnectError::internal("unexpected reply")),
        };
        Response::ok(api::SetConfigResponse {
            error: result.err().unwrap_or_default(),
            ..Default::default()
        })
    }

    #[allow(refining_impl_trait)]
    async fn watch_config(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, api::Empty>,
    ) -> ServiceResult<ServiceStream<api::Config>> {
        Response::stream_ok(watch_stream(self.shared.config_tx.subscribe()))
    }

    #[allow(refining_impl_trait)]
    async fn get_config_file(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, api::Empty>,
    ) -> ServiceResult<api::ConfigFile> {
        let yaml = serde_yaml::to_string(&redact_secrets(&self.shared.current.lock().unwrap()))
            .unwrap_or_else(|e| format!("# failed to serialize config: {e}"));
        Response::ok(api::ConfigFile {
            path: self.shared.config_path.display().to_string(),
            yaml,
            ..Default::default()
        })
    }

    #[allow(refining_impl_trait)]
    async fn verify_spotify(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, api::Empty>,
    ) -> ServiceResult<api::VerifySpotifyResponse> {
        let creds = {
            let c = self.shared.current.lock().unwrap();
            match (c.spotify.client_id.clone(), c.spotify.client_secret.clone()) {
                (Some(id), Some(secret)) if !id.is_empty() && !secret.is_empty() => {
                    Some((id, secret))
                }
                _ => None,
            }
        };
        let Some((id, secret)) = creds else {
            return Response::ok(api::VerifySpotifyResponse {
                unset: true,
                ..Default::default()
            });
        };
        // ureq is blocking, so keep it off the runtime's worker threads.
        let verdict = tokio_api::task::spawn_blocking(move || verify_spotify_credentials(&id, &secret))
            .await
            .map_err(|_| ConnectError::internal("verification task failed"))?;
        Response::ok(api::VerifySpotifyResponse {
            error: verdict.err().unwrap_or_default(),
            ..Default::default()
        })
    }
}

/// Exercise the Spotify client-credentials flow to confirm the stored keys
/// work. `Ok(())` when Spotify issues a token, `Err` with the reason otherwise
/// (bad keys, network failure, …). Credentials go in the form body — a
/// supported alternative to the Basic auth header — to avoid a base64 dep.
fn verify_spotify_credentials(client_id: &str, client_secret: &str) -> Result<(), String> {
    let response: serde_json::Value = ureq::post("https://accounts.spotify.com/api/token")
        .send_form(&[
            ("grant_type", "client_credentials"),
            ("client_id", client_id),
            ("client_secret", client_secret),
        ])
        .map_err(|e| format!("token request failed: {e}"))?
        .into_json()
        .map_err(|e| format!("token response is not json: {e}"))?;
    if response["access_token"].as_str().is_some() {
        Ok(())
    } else {
        Err("token response missing access_token".into())
    }
}

/// A copy of the config with secrets masked, for the read-only "view config
/// file" panel — so the on-disk shape is visible without exposing credentials.
fn redact_secrets(c: &Config) -> Config {
    const MASK: &str = "••••••••";
    let mask = |s: &str| {
        if s.is_empty() {
            String::new()
        } else {
            MASK.to_owned()
        }
    };
    let mask_opt = |s: &Option<String>| {
        s.as_deref()
            .filter(|s| !s.is_empty())
            .map(|_| MASK.to_owned())
    };
    let mut c = c.clone();
    c.server.password = mask(&c.server.password);
    c.spotify.client_secret = mask_opt(&c.spotify.client_secret);
    c.spotify.refresh_token = mask_opt(&c.spotify.refresh_token);
    c
}

impl UpdaterService for Api {
    #[allow(refining_impl_trait)]
    async fn apply_update(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, api::Empty>,
    ) -> ServiceResult<api::ApplyUpdateResponse> {
        let result = match self
            .shared
            .round_trip(|corr| BusCommand::ApplyUpdate { corr })
            .await?
        {
            BridgeReply::Apply(result) => result,
            _ => return Err(ConnectError::internal("unexpected reply")),
        };
        Response::ok(api::ApplyUpdateResponse {
            error: result.err().unwrap_or_default(),
            ..Default::default()
        })
    }

    #[allow(refining_impl_trait)]
    async fn watch_updater(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, api::Empty>,
    ) -> ServiceResult<ServiceStream<api::UpdaterStatus>> {
        Response::stream_ok(watch_stream(self.shared.updater_tx.subscribe()))
    }
}

impl SystemService for Api {
    #[allow(refining_impl_trait)]
    async fn restart(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, api::Empty>,
    ) -> ServiceResult<api::Empty> {
        self.shared.control.restart.store(true, Ordering::SeqCst);
        Response::ok(api::Empty::default())
    }

    #[allow(refining_impl_trait)]
    async fn quit(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, api::Empty>,
    ) -> ServiceResult<api::Empty> {
        self.shared.control.quit.store(true, Ordering::SeqCst);
        Response::ok(api::Empty::default())
    }

    #[allow(refining_impl_trait)]
    async fn open_path(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, api::OpenPathRequest>,
    ) -> ServiceResult<api::Empty> {
        let req = request.to_owned_message();
        let p = std::path::Path::new(&req.path);
        // Open the containing folder (or the path itself when it's a directory).
        // Only an existing directory is opened, so a stray request can't launch
        // arbitrary files.
        let dir = if p.is_dir() {
            Some(p.to_path_buf())
        } else {
            p.parent().map(|d| d.to_path_buf())
        };
        if let Some(dir) = dir {
            if dir.is_dir() {
                os_open(&dir.to_string_lossy());
            }
        }
        Response::ok(api::Empty::default())
    }
}

// ---------------------------------------------------------------------------
// Static SPA + media + Spotify OAuth HTTP routes

async fn serve_index() -> HttpResponse {
    asset_response("index.html")
        .unwrap_or_else(|| (StatusCode::NOT_FOUND, "web UI not built").into_response())
}

async fn serve_asset(Path(path): Path<String>) -> HttpResponse {
    asset_response(&format!("assets/{path}"))
        .unwrap_or_else(|| StatusCode::NOT_FOUND.into_response())
}

fn asset_response(path: &str) -> Option<HttpResponse> {
    let bytes = WEB_ASSETS
        .iter()
        .find(|(p, _)| *p == path)
        .map(|(_, b)| *b)?;
    Some(
        HttpResponse::builder()
            .header(header::CONTENT_TYPE, content_type(path))
            .body(Body::from(bytes.to_vec()))
            .expect("static asset response"),
    )
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}

/// GET /media?path=…: stream a finished audio file for the in-browser player,
/// honoring `Range` so the browser shows a seekable timeline. Restricted to
/// existing files with a known audio extension.
async fn serve_media(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> HttpResponse {
    let params = parse_query(query.as_deref().unwrap_or(""));
    let Some(path) = params.get("path").filter(|p| !p.is_empty()).cloned() else {
        return (StatusCode::BAD_REQUEST, "missing path").into_response();
    };
    let p = std::path::PathBuf::from(&path);
    let mime = match p
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("mp3") => "audio/mpeg",
        Some("flac") => "audio/flac",
        Some("wav") => "audio/wav",
        Some("m4a") | Some("m4b") | Some("aac") | Some("mp4") => "audio/mp4",
        Some("ogg") | Some("opus") => "audio/ogg",
        Some("aiff") | Some("aif") => "audio/aiff",
        _ => return (StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported media type").into_response(),
    };
    let config = shared.current.lock().unwrap().clone();
    let Ok(p) = allowed_media_path(&p, &config) else {
        return (StatusCode::NOT_FOUND, "").into_response();
    };
    let range_header = headers
        .get(header::RANGE)
        .and_then(|h| h.to_str().ok())
        .map(str::to_owned);
    // File stat + read of just the requested slice runs on a blocking thread so
    // it never stalls the async runtime's workers (audio files can be large).
    let read =
        tokio_api::task::spawn_blocking(move || read_media_range(&p, range_header.as_deref()))
            .await;
    let slice = match read {
        Ok(Ok(slice)) => slice,
        Ok(Err(MediaError::Status(code))) => return (code, "").into_response(),
        Ok(Err(MediaError::Unsatisfiable { total })) => {
            return HttpResponse::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::CONTENT_RANGE, format!("bytes */{total}"))
                .body(Body::empty())
                .expect("media response");
        }
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "read error").into_response(),
    };
    let mut builder = HttpResponse::builder()
        .header(header::CONTENT_TYPE, mime)
        .header(header::ACCEPT_RANGES, "bytes");
    if slice.partial {
        builder = builder.status(StatusCode::PARTIAL_CONTENT).header(
            header::CONTENT_RANGE,
            format!("bytes {}-{}/{}", slice.start, slice.end, slice.total),
        );
    }
    builder
        .body(Body::from(slice.data))
        .expect("media response")
}

fn allowed_media_path(p: &std::path::Path, config: &Config) -> Result<std::path::PathBuf, ()> {
    let p = p.canonicalize().map_err(|_| ())?;
    let roots = std::iter::once(config.sharing.download_path())
        .chain(std::iter::once(config.sharing.incomplete_path()))
        .chain(config.sharing.folders.iter().map(std::path::PathBuf::from));
    for root in roots {
        let Ok(root) = root.canonicalize() else {
            continue;
        };
        if p.starts_with(root) {
            return Ok(p);
        }
    }
    Err(())
}

struct MediaSlice {
    data: Vec<u8>,
    start: usize,
    end: usize,
    total: usize,
    partial: bool,
}

enum MediaError {
    Status(StatusCode),
    /// The client asked for bytes past the end of the file: RFC 9110 wants a
    /// 416 with `Content-Range: bytes */total`, not the whole file.
    Unsatisfiable {
        total: usize,
    },
}

/// Read only the requested byte range of an existing audio file (blocking).
/// Restricted to regular files so a stray request can't read arbitrary paths.
fn read_media_range(p: &std::path::Path, range: Option<&str>) -> Result<MediaSlice, MediaError> {
    use std::io::{Read, Seek, SeekFrom};
    let status = MediaError::Status;
    let meta = std::fs::metadata(p).map_err(|_| status(StatusCode::NOT_FOUND))?;
    if !meta.is_file() {
        return Err(status(StatusCode::NOT_FOUND));
    }
    let total = meta.len() as usize;
    let (start, end, partial) = match range.map(|h| parse_byte_range(h, total)) {
        Some(ByteRange::Slice(s, e)) => (s, e, true),
        Some(ByteRange::Unsatisfiable) => return Err(MediaError::Unsatisfiable { total }),
        // An unparseable Range header is ignored and the whole file served.
        Some(ByteRange::Ignore) | None => (0, total.saturating_sub(1), false),
    };
    if total == 0 {
        return Ok(MediaSlice {
            data: Vec::new(),
            start: 0,
            end: 0,
            total,
            partial: false,
        });
    }
    let mut file = std::fs::File::open(p).map_err(|_| status(StatusCode::INTERNAL_SERVER_ERROR))?;
    file.seek(SeekFrom::Start(start as u64))
        .map_err(|_| status(StatusCode::INTERNAL_SERVER_ERROR))?;
    let mut data = vec![0u8; end - start + 1];
    file.read_exact(&mut data)
        .map_err(|_| status(StatusCode::INTERNAL_SERVER_ERROR))?;
    Ok(MediaSlice {
        data,
        start,
        end,
        total,
        partial,
    })
}

/// GET /spotify/login: 302 to Spotify's authorize screen.
async fn spotify_login(State(shared): State<Arc<Shared>>) -> HttpResponse {
    let config = shared.current.lock().unwrap().clone();
    let Some(client_id) = config
        .spotify
        .client_id
        .as_deref()
        .filter(|s| !s.is_empty())
    else {
        return (StatusCode::BAD_REQUEST, "set your Spotify Client ID first").into_response();
    };
    let redirect_uri = format!("http://{}/spotify/callback", config.ui.bind_addr);
    let state = new_oauth_state(&shared);
    let location = format!(
        "https://accounts.spotify.com/authorize?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}",
        percent_encode(client_id),
        percent_encode(&redirect_uri),
        percent_encode("playlist-read-private playlist-read-collaborative"),
        percent_encode(&state),
    );
    HttpResponse::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, location)
        .body(Body::empty())
        .expect("redirect")
}

/// GET /spotify/callback: verify state, exchange the code for a refresh token,
/// persist it via the config store, then bounce back to the SPA.
async fn spotify_callback(
    State(shared): State<Arc<Shared>>,
    RawQuery(query): RawQuery,
) -> HttpResponse {
    match complete_spotify_login(&shared, query.as_deref().unwrap_or("")).await {
        Ok(()) => HttpResponse::builder()
            .status(StatusCode::FOUND)
            .header(header::LOCATION, "/")
            .body(Body::empty())
            .expect("redirect"),
        Err(err) => (
            StatusCode::BAD_REQUEST,
            format!("Spotify login failed: {err}"),
        )
            .into_response(),
    }
}

fn new_oauth_state(shared: &Shared) -> String {
    let seq = shared.corr.fetch_add(1, Ordering::Relaxed);
    // A loopback-only CSRF nonce; process id + a monotonic counter is enough to
    // bind a callback to a login started on this server (SystemTime is avoided
    // to stay off the wall clock in tests / determinism-sensitive contexts).
    let state = format!("{:x}{:x}", std::process::id(), seq);
    *shared.oauth_state.lock().unwrap() = Some(state.clone());
    state
}

async fn complete_spotify_login(shared: &Shared, query: &str) -> Result<(), String> {
    let params = parse_query(query);
    if let Some(err) = params.get("error") {
        return Err(format!("Spotify returned '{err}'"));
    }
    let code = params
        .get("code")
        .filter(|s| !s.is_empty())
        .ok_or("callback had no authorization code")?
        .clone();
    let returned_state = params.get("state").cloned().unwrap_or_default();
    match shared.oauth_state.lock().unwrap().take() {
        Some(expected) if expected == returned_state => {}
        _ => return Err("state mismatch — please start the login again".into()),
    }

    let mut config = shared.current.lock().unwrap().clone();
    let client_id = config
        .spotify
        .client_id
        .clone()
        .filter(|s| !s.is_empty())
        .ok_or("Spotify Client ID is not set")?;
    let client_secret = config
        .spotify
        .client_secret
        .clone()
        .filter(|s| !s.is_empty())
        .ok_or("Spotify Client Secret is not set")?;
    let redirect_uri = format!("http://{}/spotify/callback", config.ui.bind_addr);

    // The token exchange is a blocking ureq call; keep it off the async worker.
    let tokens = tokio_api::task::spawn_blocking(move || {
        crate::extract::spotify::exchange_authorization_code(
            &client_id,
            &client_secret,
            &code,
            &redirect_uri,
        )
    })
    .await
    .map_err(|e| e.to_string())??;
    config.spotify.refresh_token = Some(tokens.refresh_token);

    match shared
        .round_trip(|corr| BusCommand::SetConfig {
            corr,
            config: config::config_to_proto(&config),
        })
        .await
    {
        Ok(BridgeReply::SetConfig(result)) => result,
        Ok(_) => Err("unexpected reply".into()),
        Err(e) => Err(e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Conversions & helpers

/// Where a search card sits: which group card it renders in, and where its
/// downloads land.
struct Placement {
    group: String,
    track: u32,
    folder: String,
    prefix: String,
}

fn placement_of(searches: &api::Searches, token: u32) -> Option<Placement> {
    searches
        .searches
        .iter()
        .find(|s| s.token == token)
        .filter(|s| !s.group.is_empty())
        .map(|s| Placement {
            group: s.group.clone(),
            track: s.track,
            folder: s.folder.clone(),
            prefix: s.prefix.clone(),
        })
}

/// Turn an extracted job into bus searches, tagging each with its display group
/// and download destination.
///
/// A multi-track source (playlist, album) groups under its collection title and
/// numbers its tracks — independent of `subdir`, which only reflects the
/// "organize downloads" preference. A single search normally carries neither,
/// but when it replaces a card that had them (`inherited`) it takes that card's
/// place instead of escaping the group.
fn build_jobs(
    job: &Job,
    subdir: Option<&str>,
    inherited: Option<&Placement>,
) -> Vec<soulrust_proto::bus::SearchJob> {
    let is_bulk = job.searches.len() > 1;
    let group = job
        .folder
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| job.source_label.clone());
    let width = job.searches.len().to_string().len().max(2);
    job.searches
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let mut proto = extract::searchjob_to_proto(s);
            if let Some(folder) = subdir {
                proto.folder = folder.to_owned();
                proto.prefix = format!("{n:0width$} ", n = i + 1, width = width);
            }
            if is_bulk {
                proto.group = group.clone();
                proto.track = (i + 1) as u32;
            } else if let Some(prior) = inherited {
                proto.group = prior.group.clone();
                proto.track = prior.track;
                if !prior.folder.is_empty() {
                    proto.folder = prior.folder.clone();
                    proto.prefix = prior.prefix.clone();
                }
            }
            proto
        })
        .collect()
}

fn transfers_snapshot(downloads: &[DownloadEntry], uploads: &[UploadEntry]) -> api::Transfers {
    api::Transfers {
        downloads: downloads.iter().map(download_to_api).collect(),
        uploads: uploads.iter().map(upload_to_api).collect(),
        ..Default::default()
    }
}

fn download_to_api(d: &DownloadEntry) -> api::Download {
    let (status, place, path, error) = match &d.state {
        DownloadState::Queued => (
            api::DownloadStatus::DownloadQueued,
            0,
            String::new(),
            String::new(),
        ),
        DownloadState::Position(p) => (
            api::DownloadStatus::DownloadPosition,
            *p,
            String::new(),
            String::new(),
        ),
        DownloadState::Starting => (
            api::DownloadStatus::DownloadStarting,
            0,
            String::new(),
            String::new(),
        ),
        DownloadState::Completed(path) => (
            api::DownloadStatus::DownloadCompleted,
            0,
            path.clone(),
            String::new(),
        ),
        DownloadState::Failed(reason) => (
            api::DownloadStatus::DownloadFailed,
            0,
            String::new(),
            reason.clone(),
        ),
        DownloadState::Incomplete => (
            api::DownloadStatus::DownloadIncomplete,
            0,
            String::new(),
            String::new(),
        ),
        DownloadState::Paused => (
            api::DownloadStatus::DownloadPaused,
            0,
            String::new(),
            String::new(),
        ),
    };
    api::Download {
        username: d.username.clone(),
        filename: d.filename.clone(),
        status: status.into(),
        place,
        path,
        error,
        bytes: d.bytes,
        size: d.size,
        subdir: d.subdir.clone(),
        prefix: d.prefix.clone(),
        ..Default::default()
    }
}

fn upload_to_api(u: &UploadEntry) -> api::Upload {
    let (status, error) = match &u.state {
        UploadState::Active => (api::UploadStatus::UploadActive, String::new()),
        UploadState::Completed => (api::UploadStatus::UploadCompleted, String::new()),
        UploadState::Failed(reason) => (api::UploadStatus::UploadFailed, reason.clone()),
    };
    api::Upload {
        username: u.username.clone(),
        filename: u.filename.clone(),
        bytes: u.bytes,
        size: u.size,
        status: status.into(),
        error,
        ..Default::default()
    }
}

fn browse_listing_to_api(listing: &BrowseListingOwnedView) -> api::BrowseUserListing {
    let view = listing.view();
    api::BrowseUserListing {
        username: view.username.to_owned(),
        total_files: view.total_files,
        truncated: view.truncated,
        directories: view
            .directories
            .iter()
            .map(|dir| api::BrowseDirEntry {
                path: dir.path.to_owned(),
                files: dir
                    .files
                    .iter()
                    .map(|f| api::BrowseFileEntry {
                        name: f.name.to_owned(),
                        size: f.size,
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

fn updater_to_api(m: &UpdaterStatusChanged) -> api::UpdaterStatus {
    use api::UpdaterStatusKind as A;
    use UpdaterStatusKind as B;
    let kind = match m.kind {
        EnumValue::Known(B::UpdaterChecking) => A::UpdaterChecking,
        EnumValue::Known(B::UpdaterUpToDate) => A::UpdaterUpToDate,
        EnumValue::Known(B::UpdaterAvailable) => A::UpdaterAvailable,
        EnumValue::Known(B::UpdaterDownloading) => A::UpdaterDownloading,
        EnumValue::Known(B::UpdaterReadyToApply) => A::UpdaterReadyToApply,
        EnumValue::Known(B::UpdaterRestartRequired) => A::UpdaterRestartRequired,
        EnumValue::Known(B::UpdaterFailed) => A::UpdaterFailed,
        EnumValue::Known(B::UpdaterSkipped) => A::UpdaterSkipped,
        _ => A::UpdaterStatusKindUnspecified,
    };
    api::UpdaterStatus {
        kind: kind.into(),
        current: m.current.clone(),
        latest: m.latest.clone(),
        error: m.error.clone(),
        reason: m.reason.clone(),
        ..Default::default()
    }
}

fn config_to_api(c: &Config) -> api::Config {
    api::Config {
        server: MessageField::some(api::ServerConfig {
            host: c.server.host.clone(),
            port: u32::from(c.server.port),
            username: c.server.username.clone(),
            // Password is a secret: never sent to the client. Empty on SetConfig
            // means "keep the stored password".
            password: String::new(),
            listen_port: c.server.listen_port,
            ..Default::default()
        }),
        spotify: MessageField::some(api::SpotifyConfig {
            client_id: c.spotify.client_id.clone().unwrap_or_default(),
            // Secrets are never sent to the client: client_secret is blanked (an
            // empty value on SetConfig means "keep"), and refresh_token is off
            // the wire entirely — `connected` conveys its presence.
            client_secret: String::new(),
            connected: c
                .spotify
                .refresh_token
                .as_deref()
                .is_some_and(|s| !s.is_empty()),
            ..Default::default()
        }),
        update: MessageField::some(api::UpdateConfig {
            enabled: c.update.enabled,
            auto_apply: c.update.auto_apply,
            repo: c.update.repo.clone(),
            ..Default::default()
        }),
        ui: MessageField::some(api::UiConfig {
            bind_addr: c.ui.bind_addr.clone(),
            open_browser: c.ui.open_browser,
            ..Default::default()
        }),
        sharing: MessageField::some(api::SharingConfig {
            folders: c.sharing.folders.clone(),
            download_dir: c.sharing.download_dir.clone(),
            incomplete_dir: c.sharing.incomplete_dir.clone(),
            upload_slots: c.sharing.upload_slots,
            fifo_queue: c.sharing.fifo_queue,
            respond_to_searches: c.sharing.respond_to_searches,
            max_search_results: c.sharing.max_search_results,
            min_result_files: c.sharing.min_result_files,
            min_peer_upload_speed: c.sharing.min_peer_upload_speed,
            max_peer_queue_length: c.sharing.max_peer_queue_length,
            max_download_speed: c.sharing.max_download_speed,
            max_upload_speed: c.sharing.max_upload_speed,
            organize_downloads: c.sharing.organize_downloads,
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Reject values the on-disk config can't represent, before anything is saved.
/// The proto carries ports as `uint32`, so without this a "70000" typed into
/// Settings would wrap to 4464 and be persisted as if it were what the user
/// asked for.
fn validate_config(c: &api::Config) -> Result<(), String> {
    for (label, port) in [
        ("server port", c.server.port),
        ("listen port", c.server.listen_port),
    ] {
        if port == 0 || port > u16::MAX as u32 {
            return Err(format!("invalid {label}: {port} (must be 1–65535)"));
        }
    }
    if c.ui.bind_addr.parse::<SocketAddr>().is_err() {
        return Err(format!(
            "invalid bind address: {} (expected e.g. 127.0.0.1:5031)",
            c.ui.bind_addr
        ));
    }
    Ok(())
}

fn api_config_to_serde(c: &api::Config) -> Config {
    let opt = |s: &str| (!s.is_empty()).then(|| s.to_owned());
    Config {
        server: config::ServerConfig {
            host: c.server.host.clone(),
            // `validate_config` rejects out-of-range ports before we get here;
            // clamp rather than wrap should that ever stop being true.
            port: u16::try_from(c.server.port).unwrap_or(u16::MAX),
            username: c.server.username.clone(),
            password: c.server.password.clone(),
            listen_port: c.server.listen_port,
        },
        spotify: config::SpotifyConfig {
            client_id: opt(&c.spotify.client_id),
            client_secret: opt(&c.spotify.client_secret),
            // Not carried on the wire; set_config merges the stored token back in.
            refresh_token: None,
        },
        update: config::UpdateConfig {
            enabled: c.update.enabled,
            auto_apply: c.update.auto_apply,
            repo: c.update.repo.clone(),
        },
        ui: config::UiConfig {
            bind_addr: c.ui.bind_addr.clone(),
            open_browser: c.ui.open_browser,
        },
        sharing: config::SharingConfig {
            folders: c.sharing.folders.clone(),
            download_dir: c.sharing.download_dir.clone(),
            incomplete_dir: c.sharing.incomplete_dir.clone(),
            upload_slots: c.sharing.upload_slots,
            fifo_queue: c.sharing.fifo_queue,
            respond_to_searches: c.sharing.respond_to_searches,
            max_search_results: c.sharing.max_search_results,
            min_result_files: c.sharing.min_result_files,
            min_peer_upload_speed: c.sharing.min_peer_upload_speed,
            max_peer_queue_length: c.sharing.max_peer_queue_length,
            max_download_speed: c.sharing.max_download_speed,
            max_upload_speed: c.sharing.max_upload_speed,
            organize_downloads: c.sharing.organize_downloads,
        },
    }
}

/// Parse a `k=v&k2=v2` query string into a map, percent-decoding both sides.
fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter(|s| !s.is_empty())
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            Some((percent_decode(k), percent_decode(v)))
        })
        .collect()
}

fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi * 16 + lo) as u8);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// What a `Range` header asks for. A header we can't understand is ignored
/// (serve the whole file, 200); one we understand but can't satisfy is an
/// error the client needs to see (416).
enum ByteRange {
    Ignore,
    Unsatisfiable,
    Slice(usize, usize),
}

fn parse_byte_range(header: &str, total: usize) -> ByteRange {
    if total == 0 {
        return ByteRange::Ignore;
    }
    let Some(spec) = header.trim().strip_prefix("bytes=") else {
        return ByteRange::Ignore;
    };
    // Multi-range requests are legal but we only ever serve one range.
    if spec.contains(',') {
        return ByteRange::Ignore;
    }
    let Some((start_s, end_s)) = spec.split_once('-') else {
        return ByteRange::Ignore;
    };
    let (start, end) = if start_s.is_empty() {
        // Suffix form "-N": the last N bytes. "-0" asks for nothing.
        let Ok(n) = end_s.trim().parse::<usize>() else {
            return ByteRange::Ignore;
        };
        if n == 0 {
            return ByteRange::Unsatisfiable;
        }
        (total.saturating_sub(n), total - 1)
    } else {
        let Ok(start) = start_s.trim().parse::<usize>() else {
            return ByteRange::Ignore;
        };
        if start >= total {
            return ByteRange::Unsatisfiable;
        }
        let end = if end_s.trim().is_empty() {
            total - 1
        } else {
            match end_s.trim().parse::<usize>() {
                Ok(e) => e.min(total - 1),
                Err(_) => return ByteRange::Ignore,
            }
        };
        (start, end)
    };
    if end < start {
        return ByteRange::Unsatisfiable;
    }
    ByteRange::Slice(start, end)
}

/// Open a URL with the OS default browser, detached. Best-effort.
fn os_open(target: &str) {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut c = std::process::Command::new("open");
        c.arg(target);
        c
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", target]);
        c
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut command = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(target);
        c
    };
    let _ = command.spawn();
}

fn basename(path: &str) -> &str {
    path.rsplit(['\\', '/'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(path)
}

fn load_downloads(path: &std::path::Path) -> Vec<DownloadEntry> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn scan_disk_downloads(
    download_dir: &std::path::Path,
    incomplete_dir: &std::path::Path,
) -> Vec<DownloadEntry> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(download_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !entry.path().is_file() || name.starts_with("INCOMPLETE-") {
                continue;
            }
            out.push(DownloadEntry {
                username: String::new(),
                filename: name,
                state: DownloadState::Completed(entry.path().display().to_string()),
                bytes: 0,
                size: 0,
                subdir: String::new(),
                prefix: String::new(),
            });
        }
    }
    if let Ok(entries) = std::fs::read_dir(incomplete_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(basename) = parse_incomplete_name(&name) {
                out.push(DownloadEntry {
                    username: String::new(),
                    filename: basename,
                    state: DownloadState::Incomplete,
                    bytes: 0,
                    size: 0,
                    subdir: String::new(),
                    prefix: String::new(),
                });
            }
        }
    }
    out.truncate(MAX_DOWNLOADS);
    out
}

fn parse_incomplete_name(name: &str) -> Option<String> {
    let rest = name.strip_prefix("INCOMPLETE-")?;
    if rest.len() > 17
        && rest.as_bytes()[16] == b'-'
        && rest[..16].bytes().all(|b| b.is_ascii_hexdigit())
    {
        Some(rest[17..].to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir {
        path: std::path::PathBuf,
    }

    impl TestDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "soulrust-api-server-{name}-{}-{}",
                std::process::id(),
                std::thread::current().name().unwrap_or("test")
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn allowed_media_path_allows_configured_download_root() {
        let root = TestDir::new("allowed");
        let download_dir = root.path.join("downloads");
        std::fs::create_dir_all(&download_dir).unwrap();
        let track = download_dir.join("track.mp3");
        std::fs::write(&track, b"audio").unwrap();

        let mut config = Config::default();
        config.sharing.download_dir = download_dir.display().to_string();

        assert_eq!(
            allowed_media_path(&track, &config).unwrap(),
            track.canonicalize().unwrap()
        );
    }

    #[test]
    fn allowed_media_path_rejects_unconfigured_file() {
        let root = TestDir::new("outside");
        let download_dir = root.path.join("downloads");
        let outside_dir = root.path.join("outside");
        std::fs::create_dir_all(&download_dir).unwrap();
        std::fs::create_dir_all(&outside_dir).unwrap();
        let track = outside_dir.join("track.mp3");
        std::fs::write(&track, b"audio").unwrap();

        let mut config = Config::default();
        config.sharing.download_dir = download_dir.display().to_string();

        assert!(allowed_media_path(&track, &config).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn allowed_media_path_rejects_symlink_escape() {
        let root = TestDir::new("symlink");
        let download_dir = root.path.join("downloads");
        let outside_dir = root.path.join("outside");
        std::fs::create_dir_all(&download_dir).unwrap();
        std::fs::create_dir_all(&outside_dir).unwrap();
        let outside = outside_dir.join("track.mp3");
        std::fs::write(&outside, b"audio").unwrap();
        let link = download_dir.join("linked.mp3");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let mut config = Config::default();
        config.sharing.download_dir = download_dir.display().to_string();

        assert!(allowed_media_path(&link, &config).is_err());
    }

    fn raw_job(source_label: &str, folder: Option<&str>, queries: &[&str]) -> Job {
        Job {
            source_label: source_label.to_owned(),
            folder: folder.map(str::to_owned),
            searches: queries
                .iter()
                .map(|q| crate::extract::SearchJob {
                    raw_query: Some((*q).to_owned()),
                    ..Default::default()
                })
                .collect(),
        }
    }

    #[test]
    fn a_bulk_source_groups_and_numbers_every_track() {
        let job = raw_job("spotify playlist", Some("Summer Mix"), &["a", "b", "c"]);
        let jobs = build_jobs(&job, Some("Summer Mix"), None);

        assert_eq!(jobs.len(), 3);
        for (i, j) in jobs.iter().enumerate() {
            assert_eq!(j.group, "Summer Mix");
            assert_eq!(j.track, i as u32 + 1);
            assert_eq!(j.folder, "Summer Mix");
        }
        assert_eq!(jobs[0].prefix, "01 ");
        assert_eq!(jobs[2].prefix, "03 ");
    }

    #[test]
    fn re_searching_one_track_keeps_its_group_and_download_destination() {
        // What the card being replaced looked like.
        let searches = api::Searches {
            searches: vec![api::Search {
                token: 7,
                group: "Summer Mix".into(),
                track: 2,
                folder: "Summer Mix".into(),
                prefix: "02 ".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let prior = placement_of(&searches, 7).expect("card 7 has a placement");

        // The pencil sends a bare query, so the extractor yields a single job
        // with no collection around it.
        let job = raw_job("search", None, &["artist - better title"]);
        let jobs = build_jobs(&job, None, Some(&prior));

        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].group, "Summer Mix", "must stay in its group card");
        assert_eq!(jobs[0].track, 2);
        assert_eq!(jobs[0].folder, "Summer Mix", "must stay in the playlist folder");
        assert_eq!(jobs[0].prefix, "02 ");
    }

    #[test]
    fn a_plain_search_carries_no_group_and_ignores_a_standalone_card() {
        let searches = api::Searches {
            searches: vec![api::Search {
                token: 7,
                ..Default::default()
            }],
            ..Default::default()
        };
        // A standalone card has nothing worth inheriting.
        assert!(placement_of(&searches, 7).is_none());
        assert!(placement_of(&searches, 99).is_none());

        let jobs = build_jobs(&raw_job("search", None, &["one query"]), None, None);
        assert_eq!(jobs[0].group, "");
        assert_eq!(jobs[0].track, 0);
        assert_eq!(jobs[0].folder, "");
    }

    #[test]
    fn byte_range_past_the_end_of_the_file_is_unsatisfiable() {
        // A seek past EOF must surface as 416, not as a fresh full-file 200.
        assert!(matches!(
            parse_byte_range("bytes=500-999", 400),
            ByteRange::Unsatisfiable
        ));
        assert!(matches!(
            parse_byte_range("bytes=-0", 400),
            ByteRange::Unsatisfiable
        ));
        assert!(matches!(
            parse_byte_range("bytes=300-100", 400),
            ByteRange::Unsatisfiable
        ));
    }

    #[test]
    fn unparseable_byte_range_is_ignored_and_valid_ones_parse() {
        assert!(matches!(parse_byte_range("items=0-1", 400), ByteRange::Ignore));
        assert!(matches!(parse_byte_range("bytes=0-1,5-6", 400), ByteRange::Ignore));
        assert!(matches!(parse_byte_range("bytes=abc-", 400), ByteRange::Ignore));
        assert!(matches!(parse_byte_range("bytes=0-", 400), ByteRange::Slice(0, 399)));
        assert!(matches!(parse_byte_range("bytes=10-19", 400), ByteRange::Slice(10, 19)));
        // An end past EOF is clamped, not rejected.
        assert!(matches!(parse_byte_range("bytes=10-9999", 400), ByteRange::Slice(10, 399)));
        assert!(matches!(parse_byte_range("bytes=-50", 400), ByteRange::Slice(350, 399)));
    }

    #[test]
    fn out_of_range_ports_are_rejected_rather_than_truncated() {
        let base = config_to_api(&Config::default());
        assert!(validate_config(&base).is_ok());

        let with = |edit: fn(&mut api::ServerConfig)| {
            let mut c = base.clone();
            let mut server = (*c.server).clone();
            edit(&mut server);
            c.server = MessageField::some(server);
            c
        };

        let err = validate_config(&with(|s| s.port = 70_000)).unwrap_err();
        assert!(err.contains("server port"), "{err}");
        // Without the check, that port would be saved as 4464.
        assert_eq!(70_000_u32 as u16, 4464);

        let err = validate_config(&with(|s| s.listen_port = 0)).unwrap_err();
        assert!(err.contains("listen port"), "{err}");

        let mut c = base.clone();
        let mut ui = (*c.ui).clone();
        ui.bind_addr = "not-an-address".into();
        c.ui = MessageField::some(ui);
        assert!(validate_config(&c).unwrap_err().contains("bind address"));
    }
}
