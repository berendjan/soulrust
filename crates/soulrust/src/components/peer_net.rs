//! The async peer-network edge: a tokio reactor on a dedicated thread that both
//! *serves* incoming peer connections (browse / user-info / folder-contents)
//! and makes *outbound* connections (browse a user, deliver search results,
//! pierce a firewall). It bridges to the synchronous bus via the cloneable
//! `Writer` and an mpsc command channel its bus-facing handlers feed.
//!
//! Bulk share/browse data is built here and written straight to sockets, never
//! onto the bus; only lightweight control (commands in, `BrowseListing` /
//! `NetTx` / low-frequency `PeerActivity` out) crosses it.
//!
//! Per-connection logic ([`serve_connection`], [`browse_fetch`]) is generic
//! over `AsyncRead + AsyncWrite`, so it is unit-testable over an in-memory
//! duplex without a real socket.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rust_messenger::traits;
use rust_messenger::traits::extended::Sender;
use soulseek_proto::frame::{MAX_LARGE_PEER_MESSAGE_LEN, MAX_PEER_INIT_MESSAGE_LEN, MAX_PEER_MESSAGE_LEN};
use soulseek_proto::peer::{ConnectionType, PeerInit, PeerInitMessage, PierceFirewall};
use soulseek_proto::peer_message::{
    FileSearchResponse, GetSharedFileList, PeerMessage, SharedFileListResponse, UserInfoResponse,
};
use soulseek_proto::server::{AcceptChildren, BranchLevel, BranchRoot, ConnectToPeerRequest, ServerRequest};
use soulseek_proto::distributed::{
    self, DistribBranchLevel, DistribBranchRoot, DistribSearch, DistributedMessage,
};
use soulseek_proto::Reader;
use soulseek_proto::transfer::{
    FileTransferInit, PlaceInQueueRequest, PlaceInQueueResponse, QueueUpload, TransferDirection,
    TransferRequest, TransferResponse, UploadDenied,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::components::transfer_io;
use crate::config::AppContext;
use crate::messages::{
    BrowseDir, BrowseFailed, BrowseFile, BrowseListing, CancelDownload, ConfigChanged, DownloadComplete,
    DownloadFailed, DownloadQueuePosition, HandlerId, IncomingSearch, NetTx, PeerActivity, PeerBrowseConnect,
    DistribSpeedLimits, PeerDistribConnect, PeerDownloadConnect, PeerPierce, PeerPierceDistrib,
    PauseDownload, PeerPierceFile, PeerUploadConnect, RelayDistribSearch, ResolveUploadPeer,
    SearchResultFile, SearchResultReceived, SetExcludedPhrases, TransferProgress, UploadComplete,
    UploadFailed, UploadStarted,
};
use crate::search_response::{self, SearchFilter};
use crate::shares::ShareIndex;
use crate::transfers::uploads::{TransferId, UploadQueue};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Drop a peer connection that sends nothing for this long, so idle/slow peers
/// can't pin connection + task resources indefinitely.
const PEER_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Hard cap on a whole outbound browse exchange. A peer that connects and then
/// trickles frames resets the per-frame idle timer forever; without an overall
/// deadline the browse task (and the UI's browse-in-progress state) would never
/// resolve.
const BROWSE_DEADLINE: Duration = Duration::from_secs(120);

/// Byte budget for a browse listing forwarded onto the bus — it carries
/// *locations*, not bulk data. The bus message header is a u32 now (≈ ring/2,
/// ~2 MiB per message at the default 4 MiB ring), so this fits with headroom
/// and only the largest shares get truncated.
const MAX_LISTING_BYTES: usize = 1536 * 1024;

/// Bound on distinct pending deliveries. A searcher that never connects back
/// (offline / firewalled / malicious) would otherwise leave its queued frame in
/// the map forever; when full, the oldest pending delivery (lowest token) is
/// evicted.
const MAX_PENDING_DELIVERIES: usize = 1024;

/// Bound on in-flight transfer registries (pending/active uploads and
/// downloads). A peer that spams `QueueUpload` and never approves would
/// otherwise grow `uploads.by_token` without limit; when full, the oldest entry
/// (lowest token) is evicted.
const MAX_PENDING_TRANSFERS: usize = 1024;

/// Cap on a download's on-disk basename, so a hostile peer can't push a path
/// past the filesystem limit. Matches the 255-byte limit Nicotine+ enforces.
const MAX_BASENAME_BYTES: usize = 255;

fn file_cost(name: &str) -> usize {
    name.len() + 16
}

/// Outgoing frames awaiting an indirect (pierced) connection, keyed by the relay
/// token we minted for our `ConnectToPeer` request — the searcher echoes that
/// token back in its `PierceFirewall` when it connects in to collect them
/// (mirrors Nicotine+'s `_indirect_token_init_msgs`).
type DeliveryQueue = Mutex<PendingDeliveries>;

#[derive(Default)]
struct PendingDeliveries {
    by_token: BTreeMap<u32, Vec<Vec<u8>>>,
}

impl PendingDeliveries {
    /// Queue a frame under a relay token, evicting the oldest pending delivery
    /// if we are over budget.
    fn queue(&mut self, token: u32, frame: Vec<u8>) {
        self.by_token.entry(token).or_default().push(frame);
        while self.by_token.len() > MAX_PENDING_DELIVERIES {
            let Some((&oldest, _)) = self.by_token.iter().next() else { break };
            self.by_token.remove(&oldest);
        }
    }

    /// Remove and return the frames queued under a relay token, if any.
    fn take(&mut self, token: u32) -> Option<Vec<Vec<u8>>> {
        self.by_token.remove(&token)
    }
}

/// Work for the reactor, sent by the bus-facing handlers.
enum PeerCommand {
    Browse { username: String, ip: String, port: u16 },
    Pierce { username: String, ip: String, port: u16, token: u32 },
    /// Indirect file connection: dial back + PierceFirewall(token), then run the
    /// transfer (download receive or offered-upload serve) over that socket.
    PierceFile { username: String, ip: String, port: u16, token: u32 },
    /// Indirect distributed connection: dial back + PierceFirewall(token), then
    /// relay the peer's distributed searches.
    PierceDistrib { username: String, ip: String, port: u16, token: u32 },
    IncomingSearch { username: String, token: u32, query: String },
    Download { username: String, ip: String, port: u16, filename: String, size: u64 },
    /// Drop a pending/active download so it is no longer accepted or continued.
    CancelDownload { username: String, filename: String },
    /// A downloader approved an upload we offered; ask session to resolve their
    /// address (emitted by a connection task via the cloned sender in ConnCtx).
    StartUpload { username: String },
    /// session resolved the address; open the file connection(s) and stream.
    UploadConnect { username: String, ip: String, port: u16 },
    /// A slot freed (an upload finished); start any waiting uploads that fit.
    PumpUploads,
    /// Server-injected distributed search (we're branch root): answer it and
    /// forward it down to our children.
    RelayDistribSearch { username: String, token: u32, query: String },
    /// Store the server's distributed eligibility limits.
    SetDistribLimits { min_speed: u32, ratio: u32 },
    /// Adopt a distributed parent: open a `D` connection and relay its searches.
    DistribConnect { username: String, ip: String, port: u16 },
    /// Re-advertise our branch level/root and child-acceptance to the server
    /// (sent from connection tasks, which lack the bus writer, after our tree
    /// position or child count changes).
    AdvertiseBranch,
    /// Replace the server-supplied excluded-phrase list used to filter responses.
    SetExcludedPhrases { phrases: Vec<String> },
    /// Apply changed config live (no restart): search filter + result cap, and
    /// re-scan shared folders / update the download + incomplete dirs.
    ApplyConfig {
        live: LiveConfig,
        folders: Vec<PathBuf>,
        download_dir: PathBuf,
        incomplete_dir: PathBuf,
    },
    /// A connection task received a PlaceInQueueResponse for one of our queued
    /// downloads; forward the position to the UI (the task has no bus Writer).
    QueuePosition { username: String, filename: String, place: u32 },
    /// Throttled byte-progress for an in-flight transfer, forwarded to the UI
    /// (connection tasks have no bus Writer, so they route through here).
    TransferProgress { username: String, filename: String, bytes: u64, size: u64, upload: bool },
    /// A connection task received a filter-passing search result; forward it to
    /// the UI (the task has no bus Writer).
    SearchResult {
        token: u32,
        username: String,
        free_slots: bool,
        upload_speed: u32,
        in_queue: u32,
        files: Vec<SearchResultFile>,
    },
}

/// Throttles byte-progress for one transfer down to ~2 emissions/second (so the
/// bus sees a trickle, never per-byte) and forwards it as a `TransferProgress`
/// command. Held by a connection task and ticked from the `transfer_io` copy
/// loop; always emits the final byte so a row reaches 100%.
struct ProgressReporter {
    cmd_tx: UnboundedSender<PeerCommand>,
    username: String,
    filename: String,
    size: u64,
    upload: bool,
    last: std::time::Instant,
}

impl ProgressReporter {
    fn new(
        cmd_tx: UnboundedSender<PeerCommand>,
        username: String,
        filename: String,
        size: u64,
        upload: bool,
    ) -> Self {
        ProgressReporter { cmd_tx, username, filename, size, upload, last: std::time::Instant::now() }
    }

    fn report(&mut self, bytes: u64) {
        let now = std::time::Instant::now();
        if bytes < self.size && now.duration_since(self.last) < Duration::from_millis(500) {
            return;
        }
        self.last = now;
        let _ = self.cmd_tx.send(PeerCommand::TransferProgress {
            username: self.username.clone(),
            filename: self.filename.clone(),
            bytes,
            size: self.size,
            upload: self.upload,
        });
    }
}

/// Downloads in flight, shared across connections (the negotiation and the file
/// arrive on different sockets). Mirrors Nicotine+'s split between a request
/// keyed by user+file and the active transfer keyed by token.
#[derive(Default)]
struct Downloads {
    /// (username, filename) -> expected size, set when we send `QueueUpload`,
    /// matched when the uploader's `TransferRequest` arrives.
    pending: HashMap<(String, String), u64>,
    /// transfer token -> the download to write, set when we accept a
    /// `TransferRequest`, matched when the `F`-connection's `FileTransferInit`
    /// arrives.
    by_token: HashMap<u32, ActiveDownload>,
}

struct ActiveDownload {
    username: String,
    filename: String,
    size: u64,
}

/// Uploads we have offered, keyed by the transfer token we minted. Marked
/// `approved` once the downloader's `TransferResponse(allowed)` arrives; the
/// file connection is then opened once we resolve the peer's address.
#[derive(Default)]
struct Uploads {
    by_token: HashMap<u32, PendingUpload>,
}

struct PendingUpload {
    username: String,
    filename: String,
    real_path: PathBuf,
    size: u64,
    approved: bool,
    /// This upload's slot in the shared [`UploadQueue`], so we can report its
    /// place-in-queue and remove it from the queue when it starts or is dropped.
    transfer_id: TransferId,
}

/// Shared per-reactor state handed to every connection task.
struct ConnCtx {
    /// Swappable so a `ConfigChanged` can rescan shared folders without a
    /// restart. Readers take a cheap `Arc` clone via [`ConnCtx::shares`].
    shares: Mutex<Arc<ShareIndex>>,
    queue: Arc<DeliveryQueue>,
    downloads: Mutex<Downloads>,
    uploads: Mutex<Uploads>,
    our_username: String,
    /// Swappable (live config): where finished / partial downloads are written.
    download_dir: Mutex<PathBuf>,
    incomplete_dir: Mutex<PathBuf>,
    /// Lets connection tasks signal the reactor's command loop (e.g. an approved
    /// upload needs an address resolved and a file connection opened).
    cmd_tx: UnboundedSender<PeerCommand>,
    /// Monotonic source of transfer tokens for uploads we initiate.
    next_token: AtomicU32,
    /// Server-supplied phrases that must not appear in any file we return for a
    /// search. Updated when the server sends ExcludedSearchPhrases; read on every
    /// search response.
    excluded_phrases: Mutex<Vec<String>>,
    /// Tracks every upload we have offered but not yet started, in FIFO order, so
    /// we can answer a downloader's PlaceInQueueRequest and report an honest
    /// queue depth in search responses.
    upload_queue: Mutex<UploadQueue>,
    /// Settings the reactor re-reads on every use so a config change applies
    /// live (no restart): the requester-side search filter and the cap on files
    /// we return for an incoming search. Updated on `ConfigChanged`.
    live: Mutex<LiveConfig>,
    /// Distributed-search-tree state: our branch position and the child
    /// connections we relay searches down to.
    distrib: DistribState,
    /// Rolling average of our recent upload throughput (bytes/sec), updated as
    /// uploads complete. Advertised in search responses and user-info so peers'
    /// speed filters see an honest value instead of 0.
    upload_speed: AtomicU32,
    /// Concurrency gate: caps simultaneously-streaming uploads at the configured
    /// slot count, queueing the rest until a slot frees.
    uploads_gate: UploadGate,
}

/// A ready-to-stream upload waiting for a free slot.
struct UploadJob {
    ip: String,
    port: u16,
    token: u32,
    peer: String,
    filename: String,
    real_path: PathBuf,
    size: u64,
}

/// Limits how many uploads stream at once. `slots == 0` means unlimited.
#[derive(Default)]
struct UploadGate {
    active: AtomicU32,
    waiting: Mutex<VecDeque<UploadJob>>,
    slots: AtomicUsize,
}

impl UploadGate {
    /// Take the next waiting upload iff a slot is free, marking the slot busy.
    /// Returns `None` when at capacity or nothing is waiting.
    fn try_claim(&self) -> Option<UploadJob> {
        let slots = self.slots.load(Ordering::Relaxed);
        if slots != 0 && self.active.load(Ordering::Relaxed) as usize >= slots {
            return None;
        }
        let job = self.waiting.lock().unwrap().pop_front()?;
        self.active.fetch_add(1, Ordering::Relaxed);
        Some(job)
    }

    /// Release a slot when an upload finishes.
    fn release(&self) {
        self.active.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Our place in the distributed search tree and the children we feed.
#[derive(Default)]
struct DistribState {
    /// Our branch level + root, learned from our parent (or 0 / our own name
    /// when we're a branch root receiving searches straight from the server).
    branch: Mutex<Branch>,
    /// Child `D` connections: id -> a sender that writes raw distributed frames
    /// to that child's socket. Forwarding a search fans out over these.
    children: Mutex<HashMap<u32, UnboundedSender<Vec<u8>>>>,
    next_child_id: AtomicU32,
    /// Whether we currently have a distributed parent, or the server is feeding
    /// us searches directly (branch root). We only tell the server
    /// `AcceptChildren(true)` while attached, since a detached node has no
    /// searches to relay down.
    attached: std::sync::atomic::AtomicBool,
    /// Server distributed limits (`ParentMinSpeed` / `ParentSpeedRatio`) used to
    /// size how many children we accept from our measured upload speed.
    min_speed: AtomicU32,
    speed_ratio: AtomicU32,
}

#[derive(Clone, Default)]
struct Branch {
    level: u32,
    root: String,
}

/// Hard cap on distributed children regardless of speed (Nicotine+ uses 10).
const MAX_DISTRIB_CHILDREN: usize = 10;

/// How many children to accept, from our upload speed and the server's limits,
/// mirroring Nicotine+: `min(speed / ratio / 100, 10)` when we're fast enough,
/// otherwise 0 (a slow node shouldn't relay).
fn compute_max_children(upload_speed: u32, min_speed: u32, ratio: u32) -> usize {
    if ratio > 0 && upload_speed >= min_speed {
        ((upload_speed / ratio / 100) as usize).min(MAX_DISTRIB_CHILDREN)
    } else {
        0
    }
}

impl ConnCtx {
    /// A cheap snapshot of the current shares index (swapped on ConfigChanged).
    fn shares(&self) -> Arc<ShareIndex> {
        self.shares.lock().unwrap().clone()
    }

    fn download_dir(&self) -> PathBuf {
        self.download_dir.lock().unwrap().clone()
    }

    fn incomplete_dir(&self) -> PathBuf {
        self.incomplete_dir.lock().unwrap().clone()
    }

    /// Current child capacity from our measured upload speed + server limits.
    fn max_children(&self) -> usize {
        compute_max_children(
            self.upload_speed.load(Ordering::Relaxed),
            self.distrib.min_speed.load(Ordering::Relaxed),
            self.distrib.speed_ratio.load(Ordering::Relaxed),
        )
    }

    /// Re-encode and fan a distributed frame out to every child, pruning any
    /// whose socket-writer task has gone.
    fn forward_to_children(&self, frame: &[u8]) -> usize {
        let mut children = self.distrib.children.lock().unwrap();
        children.retain(|_, tx| tx.send(frame.to_vec()).is_ok());
        children.len()
    }

    fn branch_snapshot(&self) -> Branch {
        self.distrib.branch.lock().unwrap().clone()
    }

    fn child_count(&self) -> usize {
        self.distrib.children.lock().unwrap().len()
    }

    /// Register a child socket-writer; returns its id (used to deregister).
    fn add_child(&self, tx: UnboundedSender<Vec<u8>>) -> u32 {
        let id = self.distrib.next_child_id.fetch_add(1, Ordering::Relaxed);
        self.distrib.children.lock().unwrap().insert(id, tx);
        id
    }

    fn remove_child(&self, id: u32) {
        self.distrib.children.lock().unwrap().remove(&id);
    }

    /// Fold a finished upload's throughput into the rolling average (bytes/sec).
    /// First sample seeds it; later ones blend 70% history / 30% latest so a
    /// single slow/fast transfer doesn't swing it wildly.
    fn record_upload_speed(&self, bytes_per_sec: u32) {
        let prev = self.upload_speed.load(Ordering::Relaxed);
        let next = if prev == 0 {
            bytes_per_sec
        } else {
            ((prev as u64 * 7 + bytes_per_sec as u64 * 3) / 10) as u32
        };
        self.upload_speed.store(next, Ordering::Relaxed);
    }
}

/// The subset of config the peer-net reactor honors at runtime without a
/// restart. Refreshed wholesale when the config changes.
#[derive(Debug, Clone, Copy)]
struct LiveConfig {
    /// Filter applied to inbound search results (min files / speed / queue).
    search_filter: SearchFilter,
    /// Cap on files returned for a single incoming search.
    max_results: usize,
}

/// What a finished connection produced, for the accept loop to report on the bus.
#[derive(Debug)]
enum ConnOutcome {
    /// Served requests / negotiated only — nothing to report.
    Done,
    Downloaded { username: String, filename: String, path: String },
    DownloadFailed { username: String, filename: String, reason: String },
    /// A peer connected to collect a file we offered (the `TransferRequest`
    /// download path) and we streamed it.
    Uploaded { username: String, filename: String },
    UploadFailed { username: String, filename: String, reason: String },
}

pub struct PeerNet {
    listen_port: u16,
    folders: Vec<PathBuf>,
    username: String,
    max_search_results: usize,
    download_dir: PathBuf,
    incomplete_dir: PathBuf,
    search_filter: SearchFilter,
    fifo: bool,
    upload_slots: usize,
    cmd_tx: UnboundedSender<PeerCommand>,
    cmd_rx: Option<UnboundedReceiver<PeerCommand>>,
}

impl PeerNet {
    pub fn new<W: traits::core::Writer>(ctx: &AppContext, _writer: &W) -> Self {
        let (cmd_tx, cmd_rx) = unbounded_channel();
        // Resolve to per-OS defaults (~/Downloads/soulrust and its incomplete
        // subfolder) when unset, and make sure both exist so transfers have
        // somewhere to write.
        let download_dir = ctx.config.sharing.download_path();
        let incomplete_dir = ctx.config.sharing.incomplete_path();
        let _ = std::fs::create_dir_all(&download_dir);
        let _ = std::fs::create_dir_all(&incomplete_dir);
        // Seed the aggregate bandwidth caps from config; refreshed on ConfigChanged.
        transfer_io::set_bandwidth_limits(
            ctx.config.sharing.max_download_speed as u64,
            ctx.config.sharing.max_upload_speed as u64,
        );
        PeerNet {
            listen_port: ctx.config.server.listen_port as u16,
            folders: ctx.config.sharing.folders.iter().map(PathBuf::from).collect(),
            username: ctx.config.server.username.clone(),
            max_search_results: ctx.config.sharing.max_search_results as usize,
            download_dir,
            incomplete_dir,
            search_filter: SearchFilter {
                min_files: ctx.config.sharing.min_result_files,
                min_upload_speed: ctx.config.sharing.min_peer_upload_speed,
                max_queue_length: ctx.config.sharing.max_peer_queue_length,
            },
            fifo: ctx.config.sharing.fifo_queue,
            upload_slots: ctx.config.sharing.upload_slots as usize,
            cmd_tx,
            cmd_rx: Some(cmd_rx),
        }
    }
}

impl traits::core::Handler for PeerNet {
    type Id = HandlerId;
    const ID: HandlerId = HandlerId::PeerNet;

    fn on_start<W: traits::core::Writer>(&mut self, writer: &W) {
        let config = ReactorConfig {
            port: self.listen_port,
            folders: std::mem::take(&mut self.folders),
            username: self.username.clone(),
            max_results: self.max_search_results,
            download_dir: std::mem::take(&mut self.download_dir),
            incomplete_dir: std::mem::take(&mut self.incomplete_dir),
            search_filter: self.search_filter,
            fifo: self.fifo,
            upload_slots: self.upload_slots,
        };
        let cmd_rx = self.cmd_rx.take().expect("on_start called once");
        let cmd_tx = self.cmd_tx.clone();
        let writer = writer.clone();
        std::thread::Builder::new()
            .name("soulrust-peer-net".into())
            .spawn(move || run_reactor(config, cmd_rx, cmd_tx, writer))
            .expect("spawning peer-net reactor thread");
    }
}

/// The bus-side config snapshot handed to the reactor thread at startup.
struct ReactorConfig {
    port: u16,
    folders: Vec<PathBuf>,
    username: String,
    max_results: usize,
    download_dir: PathBuf,
    incomplete_dir: PathBuf,
    search_filter: SearchFilter,
    fifo: bool,
    upload_slots: usize,
}

impl traits::core::Handle<PeerBrowseConnect> for PeerNet {
    fn handle<W: traits::core::Writer>(&mut self, message: &PeerBrowseConnect, _writer: &W) {
        let _ = self.cmd_tx.send(PeerCommand::Browse {
            username: message.username.clone(),
            ip: message.ip.clone(),
            port: message.port as u16,
        });
    }
}

impl traits::core::Handle<PeerPierce> for PeerNet {
    fn handle<W: traits::core::Writer>(&mut self, message: &PeerPierce, _writer: &W) {
        let _ = self.cmd_tx.send(PeerCommand::Pierce {
            username: message.username.clone(),
            ip: message.ip.clone(),
            port: message.port as u16,
            token: message.token,
        });
    }
}

impl traits::core::Handle<PeerPierceFile> for PeerNet {
    fn handle<W: traits::core::Writer>(&mut self, message: &PeerPierceFile, _writer: &W) {
        let _ = self.cmd_tx.send(PeerCommand::PierceFile {
            username: message.username.clone(),
            ip: message.ip.clone(),
            port: message.port as u16,
            token: message.token,
        });
    }
}

impl traits::core::Handle<PeerPierceDistrib> for PeerNet {
    fn handle<W: traits::core::Writer>(&mut self, message: &PeerPierceDistrib, _writer: &W) {
        let _ = self.cmd_tx.send(PeerCommand::PierceDistrib {
            username: message.username.clone(),
            ip: message.ip.clone(),
            port: message.port as u16,
            token: message.token,
        });
    }
}

impl traits::core::Handle<IncomingSearch> for PeerNet {
    fn handle<W: traits::core::Writer>(&mut self, message: &IncomingSearch, _writer: &W) {
        let _ = self.cmd_tx.send(PeerCommand::IncomingSearch {
            username: message.username.clone(),
            token: message.token,
            query: message.query.clone(),
        });
    }
}

impl traits::core::Handle<SetExcludedPhrases> for PeerNet {
    fn handle<W: traits::core::Writer>(&mut self, message: &SetExcludedPhrases, _writer: &W) {
        let _ = self
            .cmd_tx
            .send(PeerCommand::SetExcludedPhrases { phrases: message.phrases.clone() });
    }
}

impl traits::core::Handle<ConfigChanged> for PeerNet {
    fn handle<W: traits::core::Writer>(&mut self, message: &ConfigChanged, _writer: &W) {
        // Apply the search filter / result cap, re-scan shared folders, and
        // update the download dirs live; only listen port + server credentials
        // still need a restart.
        let cfg = crate::config::config_from_proto(&message.config);
        let s = &cfg.sharing;
        // Bandwidth caps live in process-global token buckets the transfer tasks
        // read, so apply them directly (a relaxed atomic store) rather than
        // routing through the reactor.
        transfer_io::set_bandwidth_limits(s.max_download_speed as u64, s.max_upload_speed as u64);
        let _ = self.cmd_tx.send(PeerCommand::ApplyConfig {
            live: LiveConfig {
                search_filter: SearchFilter {
                    min_files: s.min_result_files,
                    min_upload_speed: s.min_peer_upload_speed,
                    max_queue_length: s.max_peer_queue_length,
                },
                max_results: s.max_search_results as usize,
            },
            folders: s.folders.iter().map(PathBuf::from).collect(),
            download_dir: s.download_path(),
            incomplete_dir: s.incomplete_path(),
        });
    }
}

impl traits::core::Handle<PeerDownloadConnect> for PeerNet {
    fn handle<W: traits::core::Writer>(&mut self, message: &PeerDownloadConnect, _writer: &W) {
        let _ = self.cmd_tx.send(PeerCommand::Download {
            username: message.username.clone(),
            ip: message.ip.clone(),
            port: message.port as u16,
            filename: message.filename.clone(),
            size: message.size,
        });
    }
}

impl traits::core::Handle<CancelDownload> for PeerNet {
    fn handle<W: traits::core::Writer>(&mut self, message: &CancelDownload, _writer: &W) {
        let _ = self.cmd_tx.send(PeerCommand::CancelDownload {
            username: message.username.clone(),
            filename: message.filename.clone(),
        });
    }
}

impl traits::core::Handle<PauseDownload> for PeerNet {
    fn handle<W: traits::core::Writer>(&mut self, message: &PauseDownload, _writer: &W) {
        // Pause is an abort that keeps the partial — same reactor action as
        // cancel; the UI is what distinguishes them (it keeps a Paused row).
        let _ = self.cmd_tx.send(PeerCommand::CancelDownload {
            username: message.username.clone(),
            filename: message.filename.clone(),
        });
    }
}

impl traits::core::Handle<PeerUploadConnect> for PeerNet {
    fn handle<W: traits::core::Writer>(&mut self, message: &PeerUploadConnect, _writer: &W) {
        let _ = self.cmd_tx.send(PeerCommand::UploadConnect {
            username: message.username.clone(),
            ip: message.ip.clone(),
            port: message.port as u16,
        });
    }
}

impl traits::core::Handle<PeerDistribConnect> for PeerNet {
    fn handle<W: traits::core::Writer>(&mut self, message: &PeerDistribConnect, _writer: &W) {
        let _ = self.cmd_tx.send(PeerCommand::DistribConnect {
            username: message.username.clone(),
            ip: message.ip.clone(),
            port: message.port as u16,
        });
    }
}

impl traits::core::Handle<RelayDistribSearch> for PeerNet {
    fn handle<W: traits::core::Writer>(&mut self, message: &RelayDistribSearch, _writer: &W) {
        let _ = self.cmd_tx.send(PeerCommand::RelayDistribSearch {
            username: message.username.clone(),
            token: message.token,
            query: message.query.clone(),
        });
    }
}

impl traits::core::Handle<DistribSpeedLimits> for PeerNet {
    fn handle<W: traits::core::Writer>(&mut self, message: &DistribSpeedLimits, _writer: &W) {
        let _ = self.cmd_tx.send(PeerCommand::SetDistribLimits {
            min_speed: message.min_speed,
            ratio: message.ratio,
        });
    }
}

/// One-time/low-frequency status onto the bus (listener bound, fatal errors).
/// Per-connection activity goes to stderr — it is peer-driven and must never
/// outrun the bounded bus reader.
fn status<W: traits::core::Writer>(writer: &W, note: String) {
    PeerNet::send(&PeerActivity { note, ..Default::default() }, writer);
}

fn run_reactor<W: traits::core::Writer>(
    config: ReactorConfig,
    cmd_rx: UnboundedReceiver<PeerCommand>,
    cmd_tx: UnboundedSender<PeerCommand>,
    writer: W,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(err) => {
            status(&writer, format!("peer reactor failed to start: {err}"));
            return;
        }
    };
    let port = config.port;
    runtime.block_on(async move {
        // Scan here (off the startup path) and warm the cached browse frame.
        let shares = Arc::new(ShareIndex::scan(&config.folders));
        let _ = shares.browse_frame();
        let ctx = Arc::new(ConnCtx {
            shares: Mutex::new(shares),
            queue: Arc::new(Mutex::new(PendingDeliveries::default())),
            downloads: Mutex::new(Downloads::default()),
            distrib: DistribState::default(),
            upload_speed: AtomicU32::new(0),
            uploads_gate: UploadGate::default(),
            uploads: Mutex::new(Uploads::default()),
            our_username: config.username,
            download_dir: Mutex::new(config.download_dir),
            incomplete_dir: Mutex::new(config.incomplete_dir),
            cmd_tx,
            next_token: AtomicU32::new(1),
            excluded_phrases: Mutex::new(Vec::new()),
            upload_queue: Mutex::new(UploadQueue::new(config.fifo)),
            live: Mutex::new(LiveConfig {
                search_filter: config.search_filter,
                max_results: config.max_results,
            }),
        });
        // Until we adopt a parent we're our own branch root at level 0.
        ctx.distrib.branch.lock().unwrap().root = ctx.our_username.clone();
        // Cap concurrent uploads at the configured slot count (0 = unlimited).
        ctx.uploads_gate.slots.store(config.upload_slots, Ordering::Relaxed);

        let listener = match TcpListener::bind(("0.0.0.0", port)).await {
            Ok(listener) => listener,
            Err(err) => {
                status(&writer, format!("cannot listen for peers on port {port}: {err}"));
                return;
            }
        };
        status(
            &writer,
            format!("sharing {} file(s); listening for peers on port {port}", ctx.shares().num_files()),
        );

        // Commands run concurrently with the accept loop on this single thread.
        let cmd = tokio::spawn(command_loop(cmd_rx, ctx.clone(), writer.clone()));
        accept_loop(listener, ctx, writer).await;
        cmd.abort();
    });
}

async fn accept_loop<W: traits::core::Writer>(listener: TcpListener, ctx: Arc<ConnCtx>, writer: W) {
    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                let ctx = ctx.clone();
                let writer = writer.clone();
                tokio::spawn(async move {
                    let result = serve_connection(stream, &ctx, None, |note| {
                        eprintln!("[peer-net {addr}] {note}")
                    })
                    .await;
                    report_outcome(&writer, addr, result);
                });
            }
            Err(err) => {
                // Transient (EMFILE/ECONNABORTED): log, back off, keep listening.
                eprintln!("[peer-net] accept error: {err}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// Emit a completed download (or its failure) onto the bus — one bus message per
/// finished transfer, never per byte. Per-connection errors go to stderr.
fn report_outcome<W: traits::core::Writer>(
    writer: &W,
    addr: SocketAddr,
    result: std::io::Result<ConnOutcome>,
) {
    match result {
        Ok(ConnOutcome::Done) => {}
        Ok(ConnOutcome::Downloaded { username, filename, path }) => {
            PeerNet::send(&DownloadComplete { username, filename, path, ..Default::default() }, writer);
        }
        Ok(ConnOutcome::DownloadFailed { username, filename, reason }) => {
            PeerNet::send(&DownloadFailed { username, filename, reason, ..Default::default() }, writer);
        }
        Ok(ConnOutcome::Uploaded { username, filename }) => {
            PeerNet::send(&UploadComplete { username, filename, ..Default::default() }, writer);
        }
        Ok(ConnOutcome::UploadFailed { username, filename, reason }) => {
            PeerNet::send(&UploadFailed { username, filename, reason, ..Default::default() }, writer);
        }
        Err(err) => eprintln!("[peer-net {addr}] connection ended: {err}"),
    }
}

async fn command_loop<W: traits::core::Writer>(
    mut cmd_rx: UnboundedReceiver<PeerCommand>,
    ctx: Arc<ConnCtx>,
    writer: W,
) {
    let mut connect_token: u32 = 1;
    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            PeerCommand::Browse { username: peer, ip, port } => {
                let our_username = ctx.our_username.clone();
                let writer = writer.clone();
                tokio::spawn(browse_task(ip, port, peer, our_username, writer));
            }
            PeerCommand::Pierce { username: peer, ip, port, token } => {
                let ctx = ctx.clone();
                let writer = writer.clone();
                tokio::spawn(pierce_task(ip, port, token, peer, ctx, writer));
            }
            PeerCommand::PierceFile { username: peer, ip, port, token } => {
                let ctx = ctx.clone();
                let writer = writer.clone();
                tokio::spawn(pierce_file_task(ip, port, token, peer, ctx, writer));
            }
            PeerCommand::PierceDistrib { username: peer, ip, port, token } => {
                let ctx = ctx.clone();
                tokio::spawn(pierce_distrib_task(ip, port, token, peer, ctx));
            }
            PeerCommand::Download { username: peer, ip, port, filename, size } => {
                let ctx = ctx.clone();
                let writer = writer.clone();
                tokio::spawn(download_init_task(ip, port, peer, filename, size, ctx, writer));
            }
            PeerCommand::DistribConnect { username: peer, ip, port } => {
                let ctx = ctx.clone();
                tokio::spawn(distrib_task(ip, port, peer, ctx));
            }
            PeerCommand::AdvertiseBranch => {
                // Tell the server our current tree position and whether we can
                // take (more) children — sent whenever a parent updates our
                // level/root or our child count crosses the cap.
                let branch = ctx.branch_snapshot();
                let accept = ctx.distrib.attached.load(Ordering::Relaxed)
                    && ctx.child_count() < ctx.max_children();
                PeerNet::send(&NetTx { frame: BranchLevel { level: branch.level }.to_frame(), ..Default::default() }, &writer);
                PeerNet::send(&NetTx { frame: BranchRoot { root: branch.root }.to_frame(), ..Default::default() }, &writer);
                PeerNet::send(&NetTx { frame: AcceptChildren { accept }.to_frame(), ..Default::default() }, &writer);
            }
            PeerCommand::StartUpload { username: peer } => {
                // A downloader approved an upload; ask session to resolve their
                // address so we can open the file connection.
                PeerNet::send(&ResolveUploadPeer { username: peer, ..Default::default() }, &writer);
            }
            PeerCommand::UploadConnect { username: peer, ip, port } => {
                // Collect the approved uploads for this peer.
                let ready: Vec<(u32, String, PathBuf, u64)> = {
                    let uploads = ctx.uploads.lock().unwrap();
                    uploads
                        .by_token
                        .iter()
                        .filter(|(_, u)| u.username == peer && u.approved)
                        .map(|(&token, u)| (token, u.filename.clone(), u.real_path.clone(), u.size))
                        .collect()
                };
                let offline = ip == "0.0.0.0" || port == 0;
                for (token, filename, real_path, size) in ready {
                    // The upload is starting (or failing): take it out of the
                    // pending registry and its queue slot.
                    if let Some(starting) = ctx.uploads.lock().unwrap().by_token.remove(&token) {
                        ctx.upload_queue.lock().unwrap().dequeue(starting.transfer_id);
                    }
                    if offline {
                        PeerNet::send(
                            &UploadFailed {
                                username: peer.clone(),
                                filename,
                                reason: "peer is offline or not reachable".into(), ..Default::default() },
                            &writer,
                        );
                        continue;
                    }
                    // Hand the ready transfer to the slot gate rather than
                    // streaming immediately; it starts as soon as a slot is free.
                    ctx.uploads_gate.waiting.lock().unwrap().push_back(UploadJob {
                        ip: ip.clone(),
                        port,
                        token,
                        peer: peer.clone(),
                        filename,
                        real_path,
                        size,
                    });
                }
                pump_uploads(&ctx, &writer);
            }
            PeerCommand::PumpUploads => pump_uploads(&ctx, &writer),
            PeerCommand::SetDistribLimits { min_speed, ratio } => {
                ctx.distrib.min_speed.store(min_speed, Ordering::Relaxed);
                ctx.distrib.speed_ratio.store(ratio, Ordering::Relaxed);
            }
            PeerCommand::RelayDistribSearch { username, token, query } => {
                // We're a branch root: start accepting children (capacity
                // permitting) and forward this search down, then answer it.
                if !ctx.distrib.attached.swap(true, Ordering::Relaxed) {
                    let _ = ctx.cmd_tx.send(PeerCommand::AdvertiseBranch);
                }
                let search = DistribSearch {
                    identifier: distributed::SEARCH_IDENTIFIER,
                    username: username.clone(),
                    token,
                    query: query.clone(),
                };
                ctx.forward_to_children(&search.to_frame());
                let _ = ctx.cmd_tx.send(PeerCommand::IncomingSearch { username, token, query });
            }
            PeerCommand::IncomingSearch { username: searcher, token, query } => {
                let our_token = connect_token;
                connect_token = connect_token.wrapping_add(1);
                let shares = ctx.shares();
                let queue = ctx.queue.clone();
                let writer = writer.clone();
                let our_username = ctx.our_username.clone();
                // Re-read the live result cap so a config change applies without
                // a restart.
                let max = ctx.live.lock().unwrap().max_results;
                // Snapshot the server's excluded phrases so the matcher drops any
                // file the server told us to suppress (search_response::respond
                // filters on this list).
                let excluded = ctx.excluded_phrases.lock().unwrap().clone();
                // Advertise our real upload queue depth / slot availability rather
                // than a hardcoded "always free", so requesters' speed/queue
                // filters see honest values.
                let (free_slots, in_queue) =
                    slot_advertisement(ctx.upload_queue.lock().unwrap().len());
                let upload_speed = ctx.upload_speed.load(Ordering::Relaxed);
                // Matching is CPU-bound (word-index intersection); run it off the
                // single reactor thread so a burst of network searches can't
                // head-of-line-block accept / serve / connect dispatch.
                tokio::spawn(async move {
                    let files = tokio::task::spawn_blocking(move || {
                        search_response::respond(&query, max, &excluded, &shares)
                    })
                    .await
                    .unwrap_or_default();
                    if files.is_empty() {
                        return;
                    }
                    let response = FileSearchResponse {
                        username: our_username,
                        token,
                        files,
                        free_slots,
                        // Our rolling upload throughput (0 until we've completed
                        // a transfer this run), so peers' speed filters rank us.
                        upload_speed,
                        in_queue,
                        private_files: Vec::new(),
                    };
                    // Queue under the relay token, then ask the server to relay a
                    // connect request: the searcher pierces back with this token
                    // and serve_connection delivers the queued response.
                    queue.lock().unwrap().queue(our_token, response.to_frame());
                    let request = ConnectToPeerRequest {
                        token: our_token,
                        username: searcher,
                        connection_type: ConnectionType::Peer,
                    };
                    PeerNet::send(&NetTx { frame: request.to_frame(), ..Default::default() }, &writer);
                });
            }
            PeerCommand::CancelDownload { username, filename } => {
                // Forget the request and any negotiated token so a later offer or
                // F-connection for it is dropped. A transfer already streaming to
                // disk isn't interrupted mid-write; this stops anything pending.
                let mut downloads = ctx.downloads.lock().unwrap();
                downloads.pending.remove(&(username.clone(), filename.clone()));
                downloads.by_token.retain(|_, d| !(d.username == username && d.filename == filename));
            }
            PeerCommand::SetExcludedPhrases { phrases } => {
                *ctx.excluded_phrases.lock().unwrap() = phrases;
            }
            PeerCommand::ApplyConfig { live, folders, download_dir, incomplete_dir } => {
                *ctx.live.lock().unwrap() = live;
                // Update where downloads land (creating the dirs).
                let _ = std::fs::create_dir_all(&download_dir);
                let _ = std::fs::create_dir_all(&incomplete_dir);
                *ctx.download_dir.lock().unwrap() = download_dir;
                *ctx.incomplete_dir.lock().unwrap() = incomplete_dir;
                // Re-scan shared folders off the reactor thread (walking dirs can
                // be slow), then swap the index in.
                let ctx = ctx.clone();
                let writer = writer.clone();
                tokio::spawn(async move {
                    if let Ok(index) =
                        tokio::task::spawn_blocking(move || ShareIndex::scan(&folders)).await
                    {
                        let count = index.num_files();
                        *ctx.shares.lock().unwrap() = Arc::new(index);
                        status(&writer, format!("re-scanned shares: now sharing {count} file(s)"));
                    }
                });
            }
            PeerCommand::QueuePosition { username, filename, place } => {
                PeerNet::send(&DownloadQueuePosition { username, filename, place, ..Default::default() }, &writer);
            }
            PeerCommand::SearchResult { token, username, free_slots, upload_speed, in_queue, files } => {
                PeerNet::send(
                    &SearchResultReceived { token, username, free_slots, upload_speed, in_queue, files, ..Default::default() },
                    &writer,
                );
            }
            PeerCommand::TransferProgress { username, filename, bytes, size, upload } => {
                PeerNet::send(
                    &TransferProgress { username, filename, bytes, size, upload, ..Default::default() },
                    &writer,
                );
            }
        }
    }
}

/// Honest `(free_slots, in_queue)` for a search response, derived from the
/// uploads we are currently tracking (offered + approved). We have no separate
/// slot manager yet, so this is a conservative floor — but far better than the
/// old hardcoded "always free, zero queue", which made other clients' queue
/// filters treat us as an always-available peer.
fn slot_advertisement(pending_uploads: usize) -> (bool, u32) {
    (pending_uploads == 0, pending_uploads as u32)
}

async fn connect(ip: &str, port: u16) -> std::io::Result<tokio::net::TcpStream> {
    let addr: SocketAddr = format!("{ip}:{port}")
        .parse()
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad peer address"))?;
    match tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::TcpStream::connect(addr)).await {
        Ok(result) => result,
        Err(_elapsed) => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "connect timed out")),
    }
}

/// Outbound browse: connect, request the share list, decode it, and emit the
/// result (or a failure) onto the bus.
async fn browse_task<W: traits::core::Writer>(
    ip: String,
    port: u16,
    peer: String,
    our_username: String,
    writer: W,
) {
    match browse_fetch(&ip, port, &our_username).await {
        Ok(response) => PeerNet::send(&to_listing(&peer, &response), &writer),
        Err(reason) => PeerNet::send(&BrowseFailed { username: peer, reason, ..Default::default() }, &writer),
    }
}

async fn browse_fetch(
    ip: &str,
    port: u16,
    our_username: &str,
) -> Result<SharedFileListResponse, String> {
    let mut stream = connect(ip, port).await.map_err(|e| format!("connect {ip}:{port}: {e}"))?;
    // Bound the whole exchange: a peer that trickles non-list frames keeps
    // resetting the per-frame idle timer, so only an overall deadline guarantees
    // the browse resolves.
    match tokio::time::timeout(BROWSE_DEADLINE, async {
        let init = PeerInit {
            username: our_username.to_owned(),
            connection_type: ConnectionType::Peer,
            token: 0,
        };
        stream.write_all(&init.to_frame()).await.map_err(|e| format!("send peer init: {e}"))?;
        stream
            .write_all(&GetSharedFileList.to_frame())
            .await
            .map_err(|e| format!("send share-list request: {e}"))?;

        loop {
            match read_frame_timeout(&mut stream, MAX_LARGE_PEER_MESSAGE_LEN, PEER_IDLE_TIMEOUT).await {
                Ok(Some(payload)) => match PeerMessage::decode(&payload) {
                    Ok(PeerMessage::SharedFileList(response)) => return Ok(response),
                    Ok(_) => {} // ignore other messages while awaiting the list
                    Err(err) => return Err(format!("decoding peer message: {err}")),
                },
                Ok(None) => return Err("peer closed before sending its share list".into()),
                Err(err) => return Err(format!("reading from peer: {err}")),
            }
        }
    })
    .await
    {
        Ok(result) => result,
        Err(_elapsed) => Err("peer exceeded the browse time budget".into()),
    }
}

/// Indirect connect: dial the peer and send PierceFirewall(token), then serve
/// them (the firewalled peer treats the connection as established and sends its
/// requests; the username is already known from the server's ConnectToPeer).
async fn pierce_task<W: traits::core::Writer>(
    ip: String,
    port: u16,
    token: u32,
    peer: String,
    ctx: Arc<ConnCtx>,
    writer: W,
) {
    let mut stream = match connect(&ip, port).await {
        Ok(stream) => stream,
        Err(err) => {
            eprintln!("[peer-net] pierce connect {ip}:{port} failed: {err}");
            return;
        }
    };
    if let Err(err) = stream.write_all(&PierceFirewall { token }.to_frame()).await {
        eprintln!("[peer-net] pierce send to {peer} failed: {err}");
        return;
    }
    let result = serve_connection(stream, &ctx, Some(peer.clone()), |note| {
        eprintln!("[peer-net pierce {peer}] {note}")
    })
    .await;
    report_outcome(&writer, fake_addr(), result);
}

/// Indirect **file** connect: the server told us a (firewalled) peer wants an
/// `F` connection for a transfer it can't open to us directly. We dial back,
/// send `PierceFirewall(token)`, then run the transfer over that socket — which
/// is exactly [`recv_file`]: it reads the peer's `FileTransferInit` ticket and
/// either receives a download we queued or serves an upload we offered.
async fn pierce_file_task<W: traits::core::Writer>(
    ip: String,
    port: u16,
    token: u32,
    peer: String,
    ctx: Arc<ConnCtx>,
    writer: W,
) {
    let mut stream = match connect(&ip, port).await {
        Ok(stream) => stream,
        Err(err) => {
            eprintln!("[peer-net] pierce-file connect {ip}:{port} failed: {err}");
            return;
        }
    };
    let result = pierce_and_recv_file(&mut stream, token, &ctx, &peer, &mut |note| {
        eprintln!("[peer-net pierce-file {peer}] {note}")
    })
    .await;
    report_outcome(&writer, fake_addr(), result);
}

/// Sends `PierceFirewall(token)` on a freshly dialed file connection, then hands
/// off to [`recv_file`]. Split out from [`pierce_file_task`] (which owns the
/// dial + bus reporting) so the pierce-then-transfer handshake is unit-testable
/// over an in-memory duplex.
async fn pierce_and_recv_file<S, F>(
    stream: &mut S,
    token: u32,
    ctx: &ConnCtx,
    peer: &str,
    on_activity: &mut F,
) -> std::io::Result<ConnOutcome>
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: FnMut(String),
{
    stream.write_all(&PierceFirewall { token }.to_frame()).await?;
    recv_file(stream, ctx, peer, on_activity).await
}

/// A placeholder address for outbound connections, used only to label
/// stderr/diagnostics in [`report_outcome`].
fn fake_addr() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], 0))
}

/// Outbound download: dial the peer, queue the file, and run the negotiation
/// loop so an inbound `TransferRequest` on this connection is answered. The file
/// itself arrives on a separate `F` connection (handled by [`recv_file`]).
async fn download_init_task<W: traits::core::Writer>(
    ip: String,
    port: u16,
    peer: String,
    filename: String,
    size: u64,
    ctx: Arc<ConnCtx>,
    writer: W,
) {
    let mut stream = match connect(&ip, port).await {
        Ok(stream) => stream,
        Err(err) => {
            PeerNet::send(
                &DownloadFailed {
                    username: peer,
                    filename,
                    reason: format!("connect {ip}:{port}: {err}"), ..Default::default() },
                &writer,
            );
            return;
        }
    };

    let init = PeerInit {
        username: ctx.our_username.clone(),
        connection_type: ConnectionType::Peer,
        token: 0,
    };
    let queued = async {
        stream.write_all(&init.to_frame()).await?;
        stream.write_all(&QueueUpload { file: filename.clone() }.to_frame()).await?;
        // Ask where we sit in the uploader's queue; the answer (a
        // PlaceInQueueResponse on this connection) is surfaced to the UI.
        stream.write_all(&PlaceInQueueRequest { file: filename.clone() }.to_frame()).await
    }
    .await;
    if let Err(err) = queued {
        PeerNet::send(
            &DownloadFailed { username: peer, filename, reason: format!("queueing: {err}"), ..Default::default() },
            &writer,
        );
        return;
    }

    // Record the expected transfer so the uploader's TransferRequest is matched.
    ctx.downloads.lock().unwrap().pending.insert((peer.clone(), filename.clone()), size);

    // Keep the connection open to answer the TransferRequest if it arrives here.
    let result = serve_connection(stream, &ctx, Some(peer.clone()), |note| {
        eprintln!("[peer-net download {peer}] {note}")
    })
    .await;
    report_outcome(&writer, fake_addr(), result);
}

/// Outbound upload: open a file connection to the downloader, send the peer-init
/// and `FileTransferInit`, then stream the file from disk. We are the uploader.
/// Start waiting uploads up to the free slot count. `slots == 0` means
/// unlimited. Called when new uploads are queued and whenever one finishes.
fn pump_uploads<W: traits::core::Writer>(ctx: &Arc<ConnCtx>, writer: &W) {
    while let Some(job) = ctx.uploads_gate.try_claim() {
        // Surface the upload to the UI's monitor as it starts streaming.
        PeerNet::send(
            &UploadStarted {
                username: job.peer.clone(),
                filename: job.filename.clone(),
                size: job.size, ..Default::default() },
            writer,
        );
        tokio::spawn(upload_task(
            job.ip,
            job.port,
            job.token,
            job.peer,
            job.filename,
            job.real_path,
            job.size,
            ctx.our_username.clone(),
            ctx.clone(),
            writer.clone(),
        ));
    }
}

#[allow(clippy::too_many_arguments)]
async fn upload_task<W: traits::core::Writer>(
    ip: String,
    port: u16,
    token: u32,
    peer: String,
    filename: String,
    real_path: PathBuf,
    size: u64,
    our_username: String,
    ctx: Arc<ConnCtx>,
    writer: W,
) {
    let result = async {
        let mut stream =
            connect(&ip, port).await.map_err(|e| format!("connect {ip}:{port}: {e}"))?;
        let init = PeerInit {
            username: our_username,
            connection_type: ConnectionType::File,
            token: 0,
        };
        stream.write_all(&init.to_frame()).await.map_err(|e| format!("send peer init: {e}"))?;
        let file = tokio::fs::File::open(&real_path)
            .await
            .map_err(|e| format!("open {}: {e}", real_path.display()))?;
        let started = std::time::Instant::now();
        let mut rep =
            ProgressReporter::new(ctx.cmd_tx.clone(), peer.clone(), filename.clone(), size, true);
        let sent = transfer_io::upload(&mut stream, token, file, size, &mut |b| rep.report(b))
            .await
            .map_err(|e| format!("streaming: {e}"))?;
        // Sample throughput from this transfer (ignore trivially short ones,
        // where timing is dominated by setup rather than the stream).
        let secs = started.elapsed().as_secs_f64();
        if sent >= 64 * 1024 && secs > 0.0 {
            ctx.record_upload_speed((sent as f64 / secs) as u32);
        }
        Ok::<(), String>(())
    }
    .await;

    match result {
        Ok(()) => PeerNet::send(&UploadComplete { username: peer, filename, ..Default::default() }, &writer),
        Err(reason) => PeerNet::send(&UploadFailed { username: peer, filename, reason, ..Default::default() }, &writer),
    }
    // Free the slot and let any waiting uploads start.
    ctx.uploads_gate.release();
    let _ = ctx.cmd_tx.send(PeerCommand::PumpUploads);
}

/// Reads one length-prefixed frame, returning the payload (code + contents), or
/// `None` on a clean end of stream. Rejects a declared length beyond `max_len`.
async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    max_len: usize,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > max_len {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "peer sent an oversized frame",
        ));
    }
    // Read incrementally rather than pre-allocating `len` bytes: a peer can
    // declare up to `max_len` (hundreds of MiB for a browse / search response),
    // and we must not allocate that on the strength of the length prefix alone —
    // memory tracks bytes actually delivered.
    let mut payload = Vec::new();
    let mut remaining = len;
    let mut chunk = [0u8; 16 * 1024];
    while remaining > 0 {
        let want = remaining.min(chunk.len());
        let read = reader.read(&mut chunk[..want]).await?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "peer closed mid-frame",
            ));
        }
        payload.extend_from_slice(&chunk[..read]);
        remaining -= read;
    }
    Ok(Some(payload))
}

/// [`read_frame`] with an idle timeout: `Ok(None)` if the peer sends nothing for
/// `idle`, so a silent/slow connection is dropped rather than leaking.
async fn read_frame_timeout<R: AsyncRead + Unpin>(
    reader: &mut R,
    max_len: usize,
    idle: Duration,
) -> std::io::Result<Option<Vec<u8>>> {
    match tokio::time::timeout(idle, read_frame(reader, max_len)).await {
        Ok(result) => result,
        Err(_elapsed) => Ok(None),
    }
}

/// Serves one peer connection: identify the peer (read its peer-init unless the
/// username is already known from an indirect connect), deliver any queued
/// messages for it, then answer browse / user-info / folder-contents requests
/// and transfer negotiation until it disconnects. An `F`-connection peer-init
/// instead routes to [`recv_file`] (a file we queued for download is arriving).
async fn serve_connection<S, F>(
    mut stream: S,
    ctx: &ConnCtx,
    known_peer: Option<String>,
    mut on_activity: F,
) -> std::io::Result<ConnOutcome>
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: FnMut(String),
{
    let (peer, queued) = match known_peer {
        Some(peer) => (peer, Vec::new()),
        None => {
            let Some(init_payload) =
                read_frame_timeout(&mut stream, MAX_PEER_INIT_MESSAGE_LEN, PEER_IDLE_TIMEOUT).await?
            else {
                return Ok(ConnOutcome::Done);
            };
            match PeerInitMessage::decode(&init_payload) {
                Ok(PeerInitMessage::PeerInit(init)) => {
                    if init.connection_type == ConnectionType::File {
                        // An uploader opened an F connection to deliver a file we
                        // queued for download; receive it (matched by token AND
                        // this peer-init username).
                        return recv_file(&mut stream, ctx, &init.username, &mut on_activity).await;
                    }
                    if init.connection_type == ConnectionType::Distributed {
                        // An inbound D connection is a child adopting us as its
                        // parent: feed it our branch position and forward searches
                        // down to it.
                        serve_child(&mut stream, ctx, &init.username, &mut on_activity).await?;
                        return Ok(ConnOutcome::Done);
                    }
                    (init.username, Vec::new())
                }
                Ok(PeerInitMessage::PierceFirewall(pierce)) => {
                    // Indirect connect-back: the searcher echoes the relay token
                    // we minted for our ConnectToPeer. Recover the queued
                    // delivery by that token (Nicotine+'s
                    // `_indirect_token_init_msgs`). An unknown/expired token
                    // means we never asked for this connection — drop it.
                    let Some(frames) = ctx.queue.lock().unwrap().take(pierce.token) else {
                        return Ok(ConnOutcome::Done);
                    };
                    (format!("<indirect {}>", pierce.token), frames)
                }
                Err(_) => return Ok(ConnOutcome::Done), // not a peer-init we understand
            }
        }
    };
    on_activity(format!("peer {peer} connected"));

    // Deliver anything the searcher pierced in to collect. The lock was released
    // above, before any await.
    for frame in queued {
        stream.write_all(&frame).await?;
        on_activity(format!("delivered queued response to {peer}"));
    }

    // Incoming requests are small; cap them at the medium peer limit rather than
    // the large-response cap.
    while let Some(payload) =
        read_frame_timeout(&mut stream, MAX_PEER_MESSAGE_LEN, PEER_IDLE_TIMEOUT).await?
    {
        let Ok(message) = PeerMessage::decode(&payload) else {
            break; // undecodable frame; drop the connection
        };
        match message {
            PeerMessage::SharedFileListRequest => {
                stream.write_all(ctx.shares().browse_frame()).await?;
                on_activity(format!("served browse to {peer}"));
            }
            PeerMessage::UserInfoRequest => {
                // Reflect our real upload load rather than fixed placeholders.
                let queued = ctx.upload_queue.lock().unwrap().len() as u32;
                let (free_slots, in_queue) = slot_advertisement(queued as usize);
                let info = UserInfoResponse {
                    description: format!("soulrust — {} file(s) shared", ctx.shares().num_files()),
                    picture: None,
                    total_uploads: in_queue,
                    queue_size: in_queue,
                    slots_available: free_slots,
                    upload_allowed: 1,
                };
                stream.write_all(&info.to_frame()).await?;
                on_activity(format!("served user info to {peer}"));
            }
            PeerMessage::FolderContentsRequest(request) => {
                let response = ctx.shares().folder_response(request.token, &request.directory);
                stream.write_all(&response.to_frame()).await?;
                on_activity(format!("served folder contents to {peer}"));
            }
            PeerMessage::TransferRequest(request) if request.direction == TransferDirection::Upload => {
                // The uploader is offering a file. Accept iff it matches a
                // download we queued from this peer; record it by token so the
                // F connection that follows can be matched.
                let key = (peer.clone(), request.file.clone());
                let accepted = {
                    let mut downloads = ctx.downloads.lock().unwrap();
                    // Trust the size WE recorded when queueing, not the uploader's
                    // TransferRequest.filesize (which a malicious peer could set to
                    // 0 to make us report an empty file as a complete download).
                    if let Some(size) = downloads.pending.remove(&key) {
                        downloads.by_token.insert(
                            request.token,
                            ActiveDownload {
                                username: peer.clone(),
                                filename: request.file.clone(),
                                size,
                            },
                        );
                        true
                    } else {
                        false
                    }
                };
                let response = TransferResponse {
                    token: request.token,
                    allowed: accepted,
                    filesize: None,
                    reason: if accepted { None } else { Some("Cancelled".into()) },
                };
                stream.write_all(&response.to_frame()).await?;
                if accepted {
                    on_activity(format!("accepted transfer of {} from {peer}", request.file));
                }
            }
            PeerMessage::TransferRequest(request)
                if request.direction == TransferDirection::Download =>
            {
                // A peer requests a file from us via the direct download path
                // (Soulseek.NET / slskd send `TransferRequest{Download}` rather
                // than `QueueUpload`). The downloader chose the token and, unlike
                // the QueueUpload path, *it* opens the `F` connection to collect
                // the file. So we accept on the same token and register the
                // offered upload keyed by that token — `recv_file` matches the
                // incoming `F` connection against it and streams the bytes. We do
                // NOT dial out (that would be a second, stray connection).
                match ctx.shares().resolve(&request.file) {
                    Some((path, size)) => {
                        let transfer_id = ctx.upload_queue.lock().unwrap().enqueue(&peer);
                        {
                            let mut uploads = ctx.uploads.lock().unwrap();
                            uploads.by_token.insert(
                                request.token,
                                PendingUpload {
                                    username: peer.clone(),
                                    filename: request.file.clone(),
                                    real_path: path.to_owned(),
                                    size,
                                    // Not the connect-out (`StartUpload`) path:
                                    // `approved` stays false so `UploadConnect`
                                    // never dials; the downloader connects to us.
                                    approved: false,
                                    transfer_id,
                                },
                            );
                            // Same bound as the QueueUpload path, but never evict
                            // the entry we just inserted.
                            while uploads.by_token.len() > MAX_PENDING_TRANSFERS {
                                let Some(&oldest) = uploads.by_token.keys().min() else { break };
                                if oldest == request.token {
                                    break;
                                }
                                if let Some(evicted) = uploads.by_token.remove(&oldest) {
                                    ctx.upload_queue.lock().unwrap().dequeue(evicted.transfer_id);
                                }
                            }
                        }
                        let response = TransferResponse {
                            token: request.token,
                            allowed: true,
                            filesize: Some(size),
                            reason: None,
                        };
                        stream.write_all(&response.to_frame()).await?;
                        on_activity(format!("offered {} to {peer}", request.file));
                    }
                    None => {
                        let response = TransferResponse {
                            token: request.token,
                            allowed: false,
                            filesize: None,
                            reason: Some("File not shared.".into()),
                        };
                        stream.write_all(&response.to_frame()).await?;
                    }
                }
            }
            PeerMessage::QueueUpload(queue) => {
                // A peer wants to download one of our files. Offer it (with a
                // size) if we share it; otherwise decline.
                match ctx.shares().resolve(&queue.file) {
                    Some((path, size)) => {
                        let token = ctx.next_token.fetch_add(1, Ordering::Relaxed);
                        let transfer_id = ctx.upload_queue.lock().unwrap().enqueue(&peer);
                        {
                            let mut uploads = ctx.uploads.lock().unwrap();
                            uploads.by_token.insert(
                                token,
                                PendingUpload {
                                    username: peer.clone(),
                                    filename: queue.file.clone(),
                                    real_path: path.to_owned(),
                                    size,
                                    approved: false,
                                    transfer_id,
                                },
                            );
                            // Bound the registry: a peer that offers files and
                            // never approves can't grow it without limit (evict
                            // the oldest, lowest-token pending upload — and drop
                            // its queue slot so the two stay in lockstep).
                            while uploads.by_token.len() > MAX_PENDING_TRANSFERS {
                                if let Some(&oldest) = uploads.by_token.keys().min() {
                                    if let Some(evicted) = uploads.by_token.remove(&oldest) {
                                        ctx.upload_queue.lock().unwrap().dequeue(evicted.transfer_id);
                                    }
                                } else {
                                    break;
                                }
                            }
                        }
                        let request = TransferRequest {
                            direction: TransferDirection::Upload,
                            token,
                            file: queue.file.clone(),
                            filesize: Some(size),
                        };
                        stream.write_all(&request.to_frame()).await?;
                        on_activity(format!("offered {} to {peer}", queue.file));
                    }
                    None => {
                        let denied =
                            UploadDenied { file: queue.file, reason: "File not shared".into() };
                        stream.write_all(&denied.to_frame()).await?;
                    }
                }
            }
            PeerMessage::TransferResponse(response) => {
                // A downloader's verdict on a file we offered. On approval, mark
                // it and ask the reactor to resolve the address + open the file
                // connection.
                let approved = {
                    let mut uploads = ctx.uploads.lock().unwrap();
                    match uploads.by_token.get_mut(&response.token) {
                        Some(pending) if response.allowed => {
                            pending.approved = true;
                            true
                        }
                        Some(_) => {
                            // Rejected: drop the offer and its queue slot together.
                            if let Some(rejected) = uploads.by_token.remove(&response.token) {
                                ctx.upload_queue.lock().unwrap().dequeue(rejected.transfer_id);
                            }
                            false
                        }
                        None => false,
                    }
                };
                if approved {
                    let _ = ctx.cmd_tx.send(PeerCommand::StartUpload { username: peer.clone() });
                }
            }
            PeerMessage::UploadDenied(denied) => {
                // We are the downloader: a peer we queued a file from refuses it.
                // A reason of "Queued" means remotely queued, not a rejection —
                // keep waiting for the eventual TransferRequest. (Soulseek.NET
                // trims a trailing '.' and compares case-insensitively; we match
                // that so a "Queued." reply isn't mistaken for a hard reject.)
                // Any other reason is terminal: fail fast instead of hanging.
                if denied.reason.trim_end_matches('.').eq_ignore_ascii_case("queued") {
                    on_activity(format!("{peer} queued {} remotely", denied.file));
                } else if ctx
                    .downloads
                    .lock()
                    .unwrap()
                    .pending
                    .remove(&(peer.clone(), denied.file.clone()))
                    .is_some()
                {
                    on_activity(format!("{peer} denied {}: {}", denied.file, denied.reason));
                    return Ok(ConnOutcome::DownloadFailed {
                        username: peer,
                        filename: denied.file,
                        reason: denied.reason,
                    });
                }
            }
            PeerMessage::UploadFailed(failed) => {
                // The uploader reports the transfer of a file we queued failed.
                let was_ours = ctx
                    .downloads
                    .lock()
                    .unwrap()
                    .pending
                    .remove(&(peer.clone(), failed.file.clone()))
                    .is_some();
                if was_ours {
                    on_activity(format!("{peer} reported upload of {} failed", failed.file));
                    return Ok(ConnOutcome::DownloadFailed {
                        username: peer,
                        filename: failed.file,
                        reason: "upload failed".into(),
                    });
                }
            }
            PeerMessage::PlaceInQueueRequest(req) => {
                // A downloader asks where its queued file sits. Report the FIFO
                // position of the upload we offered this peer for that file, or 0
                // if it is no longer queued (about to be sent / already dropped).
                let place = {
                    let uploads = ctx.uploads.lock().unwrap();
                    let tid = uploads
                        .by_token
                        .values()
                        .find(|u| u.username == peer && u.filename == req.file)
                        .map(|u| u.transfer_id);
                    tid.map(|id| ctx.upload_queue.lock().unwrap().place_in_queue(id) as u32)
                        .unwrap_or(0)
                };
                stream
                    .write_all(&PlaceInQueueResponse { filename: req.file.clone(), place }.to_frame())
                    .await?;
                on_activity(format!("told {peer} {} is at queue place {place}", req.file));
            }
            PeerMessage::PlaceInQueueResponse(resp) => {
                // We are the downloader: surface where our queued download sits.
                // The task has no bus Writer, so route it through the reactor.
                let _ = ctx.cmd_tx.send(PeerCommand::QueuePosition {
                    username: peer.clone(),
                    filename: resp.filename,
                    place: resp.place,
                });
            }
            PeerMessage::FileSearchResponse(resp) => {
                // We are the searcher: a peer answered one of our searches. Apply
                // the requester-side filter (min files / speed / queue) and only
                // forward survivors to the UI, where they're correlated to the
                // originating search by token.
                let accepted = ctx.live.lock().unwrap().search_filter.accepts(&resp);
                if accepted {
                    let files = resp
                        .files
                        .into_iter()
                        .map(|f| SearchResultFile {
                            bitrate: f.bitrate(),
                            length: f.length(),
                            vbr: f.is_vbr(),
                            sample_rate: f.sample_rate(),
                            bit_depth: f.bit_depth(),
                            name: f.name,
                            size: f.size, ..Default::default() })
                        .collect();
                    let _ = ctx.cmd_tx.send(PeerCommand::SearchResult {
                        token: resp.token,
                        username: resp.username,
                        free_slots: resp.free_slots,
                        upload_speed: resp.upload_speed,
                        in_queue: resp.in_queue,
                        files,
                    });
                }
            }
            _ => {} // not-yet-handled messages
        }
    }
    Ok(ConnOutcome::Done)
}

/// Receives a file arriving on an `F` connection (we are the downloader). Reads
/// the bare `FileTransferInit` token, matches it to a download we negotiated,
/// then streams the bytes to an incomplete file and moves it into place.
async fn recv_file<S, F>(
    stream: &mut S,
    ctx: &ConnCtx,
    peer_username: &str,
    on_activity: &mut F,
) -> std::io::Result<ConnOutcome>
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: FnMut(String),
{
    let mut token_buf = [0u8; FileTransferInit::LEN];
    match stream.read_exact(&mut token_buf).await {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Ok(ConnOutcome::Done)
        }
        Err(err) => return Err(err),
    }
    let token = FileTransferInit::decode(&token_buf).map(|m| m.token).unwrap_or(0);

    // Match the token AND the connecting peer's username: transfer tokens are
    // uploader-chosen and unscoped, so without the username check a peer could
    // collect a download we negotiated with someone else. Verify before
    // consuming, so a mismatch leaves the entry for the legitimate uploader.
    let active = {
        let mut downloads = ctx.downloads.lock().unwrap();
        match downloads.by_token.get(&token) {
            Some(d) if d.username == peer_username => downloads.by_token.remove(&token),
            _ => None,
        }
    };
    let Some(active) = active else {
        // Not a download we requested. It may instead be a downloader connecting
        // to collect a file we offered via `TransferRequest{Download}` — match
        // the token (and peer) against our offered uploads and serve it.
        let offered = {
            let mut uploads = ctx.uploads.lock().unwrap();
            match uploads.by_token.get(&token) {
                Some(u) if u.username == peer_username => uploads.by_token.remove(&token),
                _ => None,
            }
        };
        if let Some(up) = offered {
            ctx.upload_queue.lock().unwrap().dequeue(up.transfer_id);
            let mut rep = ProgressReporter::new(
                ctx.cmd_tx.clone(),
                up.username.clone(),
                up.filename.clone(),
                up.size,
                true,
            );
            return serve_upload(stream, up, token, on_activity, &mut |b| rep.report(b)).await;
        }
        // Unknown token, or a different peer than we negotiated with — drop it.
        return Ok(ConnOutcome::Done);
    };
    on_activity(format!("receiving {} from {} (token {token})", active.filename, active.username));

    let basename = download_basename(&active.filename);
    // Name the partial by (username, virtual path), NOT the per-attempt transfer
    // token: the token is fresh on every (re)negotiation, so a token-keyed name
    // could never be found again to resume. A stable key lets a re-queued
    // download pick up exactly where a previous attempt left off.
    let incomplete = ctx
        .incomplete_dir()
        .join(incomplete_name(&active.username, &active.filename, &basename));

    let mut rep = ProgressReporter::new(
        ctx.cmd_tx.clone(),
        active.username.clone(),
        active.filename.clone(),
        active.size,
        false,
    );
    match receive_to_disk(
        stream,
        &incomplete,
        &ctx.download_dir(),
        &basename,
        active.size,
        &mut |b| rep.report(b),
    )
    .await
    {
        Ok(path) => Ok(ConnOutcome::Downloaded {
            username: active.username,
            filename: active.filename,
            path,
        }),
        Err(reason) => {
            // Keep the partial on failure so the next attempt resumes from it
            // (the stable name above makes it findable). It is only removed once
            // the bytes are complete and the file is moved into place.
            Ok(ConnOutcome::DownloadFailed {
                username: active.username,
                filename: active.filename,
                reason,
            })
        }
    }
}

/// Serves a file on an inbound `F` connection a downloader opened to collect a
/// file we offered (the `TransferRequest{Download}` path). The peer-init and the
/// `FileTransferInit` token have already been read by the caller; here we open
/// the file, read the downloader's `FileOffset`, and stream the bytes — we must
/// NOT re-send the token, since the downloader (not us) opened the connection.
async fn serve_upload<S, F>(
    stream: &mut S,
    upload: PendingUpload,
    token: u32,
    on_activity: &mut F,
    progress: &mut (dyn FnMut(u64) + Send),
) -> std::io::Result<ConnOutcome>
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: FnMut(String),
{
    on_activity(format!("uploading {} to {} (token {token})", upload.filename, upload.username));
    let file = match tokio::fs::File::open(&upload.real_path).await {
        Ok(file) => file,
        Err(err) => {
            return Ok(ConnOutcome::UploadFailed {
                username: upload.username,
                filename: upload.filename,
                reason: format!("open {}: {err}", upload.real_path.display()),
            });
        }
    };
    match transfer_io::stream_from_offset(stream, file, upload.size, progress).await {
        Ok(_) => Ok(ConnOutcome::Uploaded { username: upload.username, filename: upload.filename }),
        Err(err) => Ok(ConnOutcome::UploadFailed {
            username: upload.username,
            filename: upload.filename,
            reason: format!("streaming: {err}"),
        }),
    }
}

/// The incomplete-file name for a download, stable across transfer attempts:
/// `INCOMPLETE-<key>-<basename>`, where `key` is a deterministic hash of the
/// (username, virtual path) pair. Two different downloads (different user or
/// path) never collide, and the *same* download always maps to the same partial
/// regardless of the transfer token, so a retry resumes it.
fn incomplete_name(username: &str, virtual_path: &str, basename: &str) -> String {
    let key = stable_key(username, virtual_path);
    format!("INCOMPLETE-{key:016x}-{basename}")
}

/// A deterministic 64-bit FNV-1a hash of `username` and `virtual_path` (with a
/// separator that cannot appear in the inputs ambiguously). Deterministic across
/// runs — unlike `DefaultHasher` — so an incomplete file is found after a
/// restart, which is what makes cross-session resume work.
fn stable_key(username: &str, virtual_path: &str) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for byte in username.bytes().chain(std::iter::once(0)).chain(virtual_path.bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// The on-disk basename for a downloaded virtual path: the last `\\`-separated
/// segment, truncated to [`MAX_BASENAME_BYTES`] (preserving the extension where
/// possible) so a hostile or pathological path can't exceed the filesystem limit.
fn download_basename(virtual_path: &str) -> String {
    let raw = virtual_path.rsplit('\\').next().unwrap_or(virtual_path);
    if raw.len() <= MAX_BASENAME_BYTES {
        return raw.to_owned();
    }
    // Keep the extension if there's a usable stem and the extension itself fits.
    if let Some((stem, ext)) = raw.rsplit_once('.') {
        let ext = format!(".{ext}");
        if !stem.is_empty() && ext.len() < MAX_BASENAME_BYTES {
            let mut end = (MAX_BASENAME_BYTES - ext.len()).min(stem.len());
            while end > 0 && !stem.is_char_boundary(end) {
                end -= 1;
            }
            return format!("{}{ext}", &stem[..end]);
        }
    }
    let mut end = MAX_BASENAME_BYTES.min(raw.len());
    while end > 0 && !raw.is_char_boundary(end) {
        end -= 1;
    }
    raw[..end].to_owned()
}

/// A path in `dir` for `basename` that does not already exist, appending
/// ` (1)`, ` (2)`, … before the extension on collision (so two downloads with
/// the same basename don't overwrite each other). Mirrors Nicotine+'s counter.
async fn unique_download_path(dir: &Path, basename: &str) -> PathBuf {
    let candidate = dir.join(basename);
    if tokio::fs::metadata(&candidate).await.is_err() {
        return candidate;
    }
    let (stem, ext) = match basename.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_owned(), format!(".{e}")),
        _ => (basename.to_owned(), String::new()),
    };
    for counter in 1.. {
        let candidate = dir.join(format!("{stem} ({counter}){ext}"));
        if tokio::fs::metadata(&candidate).await.is_err() {
            return candidate;
        }
    }
    unreachable!("counter exhausts u128 before paths")
}

/// Streams the remaining bytes of `size` from the connection into `incomplete`,
/// resuming from whatever is already on disk, then moves the completed file into
/// `download_dir` under a non-colliding name derived from `basename`. Bytes go
/// straight to disk — never the bus.
async fn receive_to_disk<S>(
    stream: &mut S,
    incomplete: &Path,
    download_dir: &Path,
    basename: &str,
    size: u64,
    progress: &mut (dyn FnMut(u64) + Send),
) -> Result<String, String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    for dir in [incomplete.parent(), Some(download_dir)].into_iter().flatten() {
        tokio::fs::create_dir_all(dir).await.map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    // Resume point: bytes already on disk from a prior attempt. A partial that
    // is somehow >= the declared size is treated as unusable and restarted from
    // scratch (a truncated re-download is safer than trusting stale bytes).
    let existing = tokio::fs::metadata(incomplete).await.map(|m| m.len()).unwrap_or(0);
    let offset = if existing < size { existing } else { 0 };

    // Append when resuming so we keep the partial; truncate for a fresh start.
    let file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(offset > 0)
        .truncate(offset == 0)
        .open(incomplete)
        .await
        .map_err(|e| format!("open {}: {e}", incomplete.display()))?;
    transfer_io::download(stream, offset, size, file, progress)
        .await
        .map_err(|e| format!("receiving: {e}"))?;
    // Resolve the collision-free destination only now the bytes are on disk.
    let final_path = unique_download_path(download_dir, basename).await;
    tokio::fs::rename(incomplete, &final_path)
        .await
        .map_err(|e| format!("move to final: {e}"))?;
    Ok(final_path.display().to_string())
}

/// Relays a distributed (`D`) connection's searches: reads distributed frames
/// and, for each `DistribSearch` (or an embedded one), feeds it into the same
/// responder path as a server search via the reactor command channel. Used for
/// both a parent we adopt and a child that connects to us.
async fn serve_distrib<S, F>(
    stream: &mut S,
    ctx: &ConnCtx,
    peer: &str,
    on_activity: &mut F,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: FnMut(String),
{
    on_activity(format!("distributed parent {peer} connected"));
    ctx.distrib.attached.store(true, Ordering::Relaxed);
    let result = serve_distrib_loop(stream, ctx, peer, on_activity).await;
    // Parent gone: revert to being our own branch root and stop accepting
    // children (and tell the server + any children).
    ctx.distrib.attached.store(false, Ordering::Relaxed);
    {
        let mut branch = ctx.distrib.branch.lock().unwrap();
        branch.level = 0;
        branch.root = ctx.our_username.clone();
    }
    ctx.forward_to_children(&DistribBranchLevel { level: 0 }.to_frame());
    ctx.forward_to_children(
        &DistribBranchRoot { root_username: ctx.our_username.clone() }.to_frame(),
    );
    let _ = ctx.cmd_tx.send(PeerCommand::AdvertiseBranch);
    result
}

/// The read loop for a distributed parent connection, split out so the caller
/// can reset our branch state once it ends however it ends.
async fn serve_distrib_loop<S, F>(
    stream: &mut S,
    ctx: &ConnCtx,
    peer: &str,
    on_activity: &mut F,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: FnMut(String),
{
    while let Some(payload) =
        read_frame_timeout(stream, MAX_PEER_MESSAGE_LEN, PEER_IDLE_TIMEOUT).await?
    {
        match DistributedMessage::decode(&payload) {
            Ok(DistributedMessage::Search(search)) => relay_distrib_search(ctx, search, on_activity),
            Ok(DistributedMessage::Embedded(embedded))
                if embedded.inner_code == distributed::code::SEARCH =>
            {
                if let Ok(search) = DistribSearch::decode(&mut Reader::new(&embedded.inner_message)) {
                    relay_distrib_search(ctx, search, on_activity);
                }
            }
            Ok(DistributedMessage::BranchLevel(bl)) => {
                // Our level is the parent's level + 1. Record it, push it to our
                // children, and re-advertise to the server.
                let level = bl.level.max(0) as u32 + 1;
                ctx.distrib.branch.lock().unwrap().level = level;
                ctx.forward_to_children(&DistribBranchLevel { level: level as i32 }.to_frame());
                let _ = ctx.cmd_tx.send(PeerCommand::AdvertiseBranch);
                on_activity(format!("branch level {level} via {peer}"));
            }
            Ok(DistributedMessage::BranchRoot(br)) => {
                ctx.distrib.branch.lock().unwrap().root = br.root_username.clone();
                ctx.forward_to_children(
                    &DistribBranchRoot { root_username: br.root_username }.to_frame(),
                );
                let _ = ctx.cmd_tx.send(PeerCommand::AdvertiseBranch);
            }
            Ok(_) => {}      // ping / child-depth — informational
            Err(_) => break, // undecodable frame; drop the connection
        }
    }
    Ok(())
}

/// Forward a distributed search verbatim to all our children, then respond to it
/// from our shares (same path as a server search).
fn relay_distrib_search<F: FnMut(String)>(ctx: &ConnCtx, search: DistribSearch, on_activity: &mut F) {
    let children = ctx.forward_to_children(&search.to_frame());
    let _ = ctx.cmd_tx.send(PeerCommand::IncomingSearch {
        username: search.username,
        token: search.token,
        query: search.query,
    });
    on_activity(format!("relayed a distributed search to {children} child(ren)"));
}

/// Serve an inbound distributed child: send it our branch position, register it
/// for search forwarding, and pump forwarded frames to its socket until it
/// disconnects. Searches flow down-tree (parent → child), so we mostly write;
/// reads are drained and discarded (only used to notice the child leaving).
async fn serve_child<S, F>(
    stream: &mut S,
    ctx: &ConnCtx,
    peer: &str,
    on_activity: &mut F,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: FnMut(String),
{
    if ctx.child_count() >= ctx.max_children() {
        on_activity(format!("declining distributed child {peer} (at capacity)"));
        return Ok(());
    }
    let (tx, mut rx) = unbounded_channel::<Vec<u8>>();
    let id = ctx.add_child(tx);
    let _ = ctx.cmd_tx.send(PeerCommand::AdvertiseBranch); // child count changed
    on_activity(format!("adopted distributed child {peer}"));

    // Tell the new child our branch position so it can set its own level/root.
    let branch = ctx.branch_snapshot();
    stream.write_all(&DistribBranchLevel { level: branch.level as i32 }.to_frame()).await?;
    stream.write_all(&DistribBranchRoot { root_username: branch.root }.to_frame()).await?;

    // Searches flow down-tree (parent → child), so we only write to a child:
    // each forwarded frame, plus a periodic ping so a vanished child's socket
    // error is noticed even when no searches are flowing.
    loop {
        let next = tokio::time::timeout(PEER_IDLE_TIMEOUT, rx.recv()).await;
        let frame = match next {
            Ok(Some(frame)) => frame,
            Ok(None) => break,                                   // we were deregistered
            Err(_) => distributed::DistribPing.to_frame(),       // keepalive probe
        };
        if stream.write_all(&frame).await.is_err() {
            break;
        }
    }
    ctx.remove_child(id);
    let _ = ctx.cmd_tx.send(PeerCommand::AdvertiseBranch); // freed a slot
    on_activity(format!("distributed child {peer} left"));
    Ok(())
}

/// Adopt a distributed parent: dial it, send our peer-init for a `D` connection,
/// then relay the searches it sends down to us.
async fn distrib_task(ip: String, port: u16, peer: String, ctx: Arc<ConnCtx>) {
    let mut stream = match connect(&ip, port).await {
        Ok(stream) => stream,
        Err(err) => {
            eprintln!("[peer-net] distrib connect {ip}:{port} failed: {err}");
            return;
        }
    };
    let init = PeerInit {
        username: ctx.our_username.clone(),
        connection_type: ConnectionType::Distributed,
        token: 0,
    };
    if let Err(err) = stream.write_all(&init.to_frame()).await {
        eprintln!("[peer-net] distrib init to {peer} failed: {err}");
        return;
    }
    let result =
        serve_distrib(&mut stream, &ctx, &peer, &mut |note| eprintln!("[peer-net distrib {peer}] {note}"))
            .await;
    if let Err(err) = result {
        eprintln!("[peer-net distrib {peer}] ended: {err}");
    }
}

/// Indirect distributed connect: the server relayed a ConnectToPeer(D) from a
/// (firewalled) peer that wants to join the search tree with us. We dial back,
/// send `PierceFirewall(token)` (instead of a peer-init), then relay its
/// distributed searches just like an adopted parent or an inbound child.
async fn pierce_distrib_task(ip: String, port: u16, token: u32, peer: String, ctx: Arc<ConnCtx>) {
    let mut stream = match connect(&ip, port).await {
        Ok(stream) => stream,
        Err(err) => {
            eprintln!("[peer-net] pierce-distrib connect {ip}:{port} failed: {err}");
            return;
        }
    };
    if let Err(err) = pierce_and_serve_distrib(&mut stream, token, &ctx, &peer, &mut |note| {
        eprintln!("[peer-net pierce-distrib {peer}] {note}")
    })
    .await
    {
        eprintln!("[peer-net pierce-distrib {peer}] ended: {err}");
    }
}

/// Sends `PierceFirewall(token)` on a freshly dialed distributed connection,
/// then hands off to [`serve_distrib`]. Split out from [`pierce_distrib_task`]
/// (which owns the dial) so the pierce-then-relay handshake is unit-testable
/// over an in-memory duplex.
async fn pierce_and_serve_distrib<S, F>(
    stream: &mut S,
    token: u32,
    ctx: &ConnCtx,
    peer: &str,
    on_activity: &mut F,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: FnMut(String),
{
    stream.write_all(&PierceFirewall { token }.to_frame()).await?;
    serve_distrib(stream, ctx, peer, on_activity).await
}

/// Maps a decoded share list to the bus message, capping the forwarded listing
/// to a byte budget while still reporting the true file count.
fn to_listing(username: &str, response: &SharedFileListResponse) -> BrowseListing {
    let all_dirs = response.directories.iter().chain(response.private_directories.iter());
    let total_files: u64 = all_dirs.clone().map(|d| d.files.len() as u64).sum();

    let mut directories = Vec::new();
    let mut budget = MAX_LISTING_BYTES;
    let mut truncated = false;
    'dirs: for dir in all_dirs {
        budget = budget.saturating_sub(dir.path.len() + 8);
        let mut files = Vec::new();
        for file in &dir.files {
            let cost = file_cost(&file.name);
            if cost > budget {
                truncated = true;
                directories.push(BrowseDir { path: dir.path.clone(), files, ..Default::default() });
                break 'dirs;
            }
            budget -= cost;
            files.push(BrowseFile {
                name: file.name.clone(),
                size: file.size,
                ..Default::default()
            });
        }
        directories.push(BrowseDir { path: dir.path.clone(), files, ..Default::default() });
    }

    BrowseListing {
        username: username.to_owned(),
        directories,
        total_files,
        truncated,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soulseek_proto::peer::{PeerInit, PierceFirewall};
    use soulseek_proto::peer_message::{FolderContentsRequest, SharedDirectory, SharedFile, UserInfoRequest};

    fn test_index() -> ShareIndex {
        let mut index = ShareIndex::default();
        index.add_virtual("Music\\Album\\song.mp3", 4096);
        index.add_virtual("Music\\Album\\other.flac", 8192);
        index
    }

    fn test_ctx() -> Arc<ConnCtx> {
        let (cmd_tx, cmd_rx) = unbounded_channel();
        // Keep the channel open for the test's lifetime without a live consumer;
        // sends from the connection task just go nowhere.
        std::mem::forget(cmd_rx);
        Arc::new(ConnCtx {
            shares: Mutex::new(Arc::new(test_index())),
            queue: Arc::new(Mutex::new(PendingDeliveries::default())),
            downloads: Mutex::new(Downloads::default()),
            distrib: DistribState::default(),
            upload_speed: AtomicU32::new(0),
            uploads_gate: UploadGate::default(),
            uploads: Mutex::new(Uploads::default()),
            our_username: "me".into(),
            download_dir: Mutex::new(std::env::temp_dir()),
            incomplete_dir: Mutex::new(std::env::temp_dir()),
            cmd_tx,
            next_token: AtomicU32::new(1),
            excluded_phrases: Mutex::new(Vec::new()),
            upload_queue: Mutex::new(UploadQueue::new(false)),
            live: Mutex::new(LiveConfig {
                search_filter: SearchFilter { min_files: 1, min_upload_speed: 0, max_queue_length: 0 },
                max_results: 100,
            }),
        })
    }

    async fn read_one_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Vec<u8> {
        read_frame(reader, MAX_LARGE_PEER_MESSAGE_LEN).await.unwrap().unwrap()
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }

    #[test]
    fn serves_browse_user_info_and_folder() {
        runtime().block_on(async {
            let (mut client, server) = tokio::io::duplex(64 * 1024);
            let ctx = test_ctx();
            let serve = tokio::spawn(async move {
                serve_connection(server, &ctx, None, |_| {}).await
            });

            let init = PeerInit { username: "peer".into(), connection_type: ConnectionType::Peer, token: 0 };
            client.write_all(&init.to_frame()).await.unwrap();
            client.write_all(&GetSharedFileList.to_frame()).await.unwrap();
            client.write_all(&UserInfoRequest.to_frame()).await.unwrap();
            client.write_all(&FolderContentsRequest { token: 7, directory: "Music\\Album".into() }.to_frame()).await.unwrap();

            let browse = PeerMessage::decode(&read_one_frame(&mut client).await).unwrap();
            assert!(matches!(browse, PeerMessage::SharedFileList(_)));
            let info = PeerMessage::decode(&read_one_frame(&mut client).await).unwrap();
            let PeerMessage::UserInfoResponse(ui) = info else { panic!("expected user info") };
            // Real values from the (empty) upload queue, not fixed placeholders.
            assert!(ui.slots_available, "free slots advertised when the queue is empty");
            assert_eq!(ui.queue_size, 0);
            let folder = PeerMessage::decode(&read_one_frame(&mut client).await).unwrap();
            let PeerMessage::FolderContents(fc) = folder else { panic!("expected folder contents") };
            assert_eq!(fc.token, 7);
            assert_eq!(fc.folders[0].files.len(), 2);

            drop(client);
            serve.await.unwrap().unwrap();
        });
    }

    #[test]
    fn delivers_queued_response_to_a_pierced_searcher() {
        // A FileSearchResponse queued under relay token 7 is delivered when the
        // searcher connects in and pierces with that token — the real
        // search-delivery path (the searcher echoes our ConnectToPeer token in a
        // PierceFirewall, not a PeerInit).
        runtime().block_on(async {
            let (mut client, server) = tokio::io::duplex(64 * 1024);
            let ctx = test_ctx();
            let response = FileSearchResponse {
                username: "us".into(),
                token: 99,
                files: vec![SharedFile { name: "hit.mp3".into(), size: 1, extension: String::new(), attributes: vec![] }],
                free_slots: true,
                upload_speed: 0,
                in_queue: 0,
                private_files: vec![],
            };
            ctx.queue.lock().unwrap().queue(7, response.to_frame());

            let serve = tokio::spawn(async move {
                serve_connection(server, &ctx, None, |_| {}).await
            });
            client.write_all(&PierceFirewall { token: 7 }.to_frame()).await.unwrap();

            let delivered = PeerMessage::decode(&read_one_frame(&mut client).await).unwrap();
            let PeerMessage::FileSearchResponse(resp) = delivered else { panic!("expected search response") };
            assert_eq!(resp.token, 99);
            assert_eq!(resp.files[0].name, "hit.mp3");

            drop(client);
            serve.await.unwrap().unwrap();
        });
    }

    #[test]
    fn unknown_pierce_token_is_dropped() {
        // A PierceFirewall carrying a token we never issued (no queued delivery)
        // is dropped without serving — matches Nicotine+ closing connections for
        // expired/unknown indirect tokens.
        runtime().block_on(async {
            let (mut client, server) = tokio::io::duplex(64 * 1024);
            let ctx = test_ctx();
            let serve = tokio::spawn(async move {
                serve_connection(server, &ctx, None, |_| {}).await
            });
            client.write_all(&PierceFirewall { token: 7 }.to_frame()).await.unwrap();
            serve.await.unwrap().unwrap();

            // The server closed the connection: the client reads EOF.
            let mut buf = [0u8; 1];
            assert_eq!(client.read(&mut buf).await.unwrap(), 0);
        });
    }

    #[test]
    fn pierced_connection_serves_without_a_peer_init() {
        // known_peer set (as after PierceFirewall): no peer-init is read; the
        // peer sends a request directly and we serve it.
        runtime().block_on(async {
            let (mut client, server) = tokio::io::duplex(64 * 1024);
            let ctx = test_ctx();
            let serve = tokio::spawn(async move {
                serve_connection(server, &ctx, Some("fw-peer".into()), |_| {}).await
            });
            client.write_all(&GetSharedFileList.to_frame()).await.unwrap();
            let browse = PeerMessage::decode(&read_one_frame(&mut client).await).unwrap();
            assert!(matches!(browse, PeerMessage::SharedFileList(_)));
            drop(client);
            serve.await.unwrap().unwrap();
        });
    }

    #[test]
    fn browse_fetch_reads_a_served_share_list() {
        // browse_fetch (outbound) against a stub peer that serves a list.
        runtime().block_on(async {
            let (client, mut server) = tokio::io::duplex(64 * 1024);
            // Stub peer: read our handshake, then serve a share list.
            let stub = tokio::spawn(async move {
                // drain peer-init + request
                let _ = read_frame(&mut server, MAX_PEER_INIT_MESSAGE_LEN).await.unwrap();
                let _ = read_frame(&mut server, MAX_PEER_MESSAGE_LEN).await.unwrap();
                let list = SharedFileListResponse {
                    directories: vec![SharedDirectory {
                        path: "Music".into(),
                        files: vec![SharedFile { name: "a.mp3".into(), size: 5, extension: String::new(), attributes: vec![] }],
                    }],
                    private_directories: vec![],
                };
                server.write_all(&list.to_frame()).await.unwrap();
            });
            // Drive browse_fetch over the client end via a tiny shim: reuse the
            // inner exchange by writing init+request and reading.
            let mut client = client;
            let init = PeerInit { username: "me".into(), connection_type: ConnectionType::Peer, token: 0 };
            client.write_all(&init.to_frame()).await.unwrap();
            client.write_all(&GetSharedFileList.to_frame()).await.unwrap();
            let resp = loop {
                let p = read_frame(&mut client, MAX_LARGE_PEER_MESSAGE_LEN).await.unwrap().unwrap();
                if let PeerMessage::SharedFileList(r) = PeerMessage::decode(&p).unwrap() {
                    break r;
                }
            };
            let listing = to_listing("alice", &resp);
            assert_eq!(listing.total_files, 1);
            assert_eq!(listing.directories[0].files[0].name, "a.mp3");
            stub.await.unwrap();
        });
    }

    #[test]
    fn accepts_a_transfer_request_matching_a_pending_download() {
        // We queued a download from "bob"; bob's TransferRequest is accepted and
        // recorded by token for the F connection that follows.
        use soulseek_proto::transfer::TransferRequest;
        runtime().block_on(async {
            let (mut client, server) = tokio::io::duplex(64 * 1024);
            let ctx = test_ctx();
            ctx.downloads
                .lock()
                .unwrap()
                .pending
                .insert(("bob".into(), "Music\\f.mp3".into()), 5);
            let ctx_serve = ctx.clone();
            let serve = tokio::spawn(async move {
                serve_connection(server, &ctx_serve, Some("bob".into()), |_| {}).await
            });

            client
                .write_all(
                    &TransferRequest {
                        direction: TransferDirection::Upload,
                        token: 42,
                        file: "Music\\f.mp3".into(),
                        filesize: Some(5),
                    }
                    .to_frame(),
                )
                .await
                .unwrap();

            let reply = PeerMessage::decode(&read_one_frame(&mut client).await).unwrap();
            let PeerMessage::TransferResponse(resp) = reply else { panic!("expected transfer response") };
            assert_eq!(resp.token, 42);
            assert!(resp.allowed, "a matching pending download is accepted");

            drop(client);
            serve.await.unwrap().unwrap();
            assert!(ctx.downloads.lock().unwrap().by_token.contains_key(&42), "recorded by token");
        });
    }

    #[test]
    fn receives_a_queued_file_on_an_f_connection() {
        // An F connection whose FileTransferInit token matches a negotiated
        // download streams to disk and lands in the download dir.
        runtime().block_on(async {
            let dir = std::env::temp_dir().join(format!("soulrust-dl-{}", std::process::id()));
            let _ = std::fs::create_dir_all(&dir);
            let (cmd_tx, cmd_rx) = unbounded_channel();
            std::mem::forget(cmd_rx);
            let ctx = Arc::new(ConnCtx {
                shares: Mutex::new(Arc::new(test_index())),
                queue: Arc::new(Mutex::new(PendingDeliveries::default())),
                downloads: Mutex::new(Downloads::default()),
            distrib: DistribState::default(),
            upload_speed: AtomicU32::new(0),
            uploads_gate: UploadGate::default(),
                uploads: Mutex::new(Uploads::default()),
                our_username: "me".into(),
                download_dir: Mutex::new(dir.clone()),
                incomplete_dir: Mutex::new(dir.clone()),
                cmd_tx,
                next_token: AtomicU32::new(1),
                excluded_phrases: Mutex::new(Vec::new()),
                upload_queue: Mutex::new(UploadQueue::new(false)),
                live: Mutex::new(LiveConfig {
                    search_filter: SearchFilter { min_files: 1, min_upload_speed: 0, max_queue_length: 0 },
                    max_results: 100,
                }),
            });
            ctx.downloads.lock().unwrap().by_token.insert(
                42,
                ActiveDownload { username: "bob".into(), filename: "Music\\got.mp3".into(), size: 11 },
            );

            let (mut client, server) = tokio::io::duplex(64 * 1024);
            let serve = tokio::spawn(async move {
                serve_connection(server, &ctx, None, |_| {}).await
            });

            let init = PeerInit { username: "bob".into(), connection_type: ConnectionType::File, token: 0 };
            client.write_all(&init.to_frame()).await.unwrap();
            client.write_all(&FileTransferInit { token: 42 }.to_bytes()).await.unwrap();
            // Read our FileOffset (8 bytes), then send the 11-byte file.
            let mut offset = [0u8; 8];
            client.read_exact(&mut offset).await.unwrap();
            client.write_all(b"hello world").await.unwrap();
            drop(client);

            let outcome = serve.await.unwrap().unwrap();
            let ConnOutcome::Downloaded { filename, path, .. } = outcome else {
                panic!("expected a completed download");
            };
            assert_eq!(filename, "Music\\got.mp3");
            assert_eq!(std::fs::read(&path).unwrap(), b"hello world");
            let _ = std::fs::remove_file(&path);
        });
    }

    /// A fresh, uniquely-named temp directory for a resume test (no `tempfile`
    /// dependency; the caller cleans it up).
    fn unique_dir(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("soulrust-{tag}-{}-{n}", std::process::id()))
    }

    #[test]
    fn incomplete_name_is_stable_per_user_and_path() {
        // The same (user, path) must always map to the same partial name so a
        // retry resumes it; different users or paths must not collide.
        let a = incomplete_name("bob", "Music\\x.mp3", "x.mp3");
        assert_eq!(a, incomplete_name("bob", "Music\\x.mp3", "x.mp3"), "stable across calls");
        assert_ne!(a, incomplete_name("alice", "Music\\x.mp3", "x.mp3"), "different user differs");
        assert_ne!(a, incomplete_name("bob", "Music\\y.mp3", "y.mp3"), "different path differs");
        assert!(a.starts_with("INCOMPLETE-") && a.ends_with("x.mp3"));
        // The key is independent of the transfer token (not part of the inputs).
        assert!(!a.contains("token"));
    }

    #[test]
    fn receive_to_disk_resumes_from_an_existing_partial() {
        // A partial from a prior attempt is kept and the transfer resumes from
        // its length: we send that offset and append the remaining bytes.
        runtime().block_on(async {
            let dir = unique_dir("resume");
            std::fs::create_dir_all(&dir).unwrap();
            let incomplete = dir.join("INCOMPLETE-abc-song.mp3");
            let full = b"abcdefghijklmnopqrstuvwxyz".to_vec(); // 26 bytes
            tokio::fs::write(&incomplete, &full[..10]).await.unwrap(); // 10 already on disk

            let (mut client, mut server) = tokio::io::duplex(64 * 1024);
            let dir2 = dir.clone();
            let size = full.len() as u64;
            let recv = tokio::spawn(async move {
                receive_to_disk(&mut server, &incomplete, &dir2, "song.mp3", size, &mut |_| {}).await
            });

            // Uploader side: read the resume offset, then stream the remainder.
            let mut off = [0u8; 8];
            client.read_exact(&mut off).await.unwrap();
            assert_eq!(u64::from_le_bytes(off), 10, "resume offset == existing partial length");
            client.write_all(&full[10..]).await.unwrap();
            drop(client);

            let path = recv.await.unwrap().unwrap();
            assert_eq!(tokio::fs::read(&path).await.unwrap(), full, "partial + resumed == full file");
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn receive_to_disk_restarts_when_partial_exceeds_declared_size() {
        // A partial at least as large as the declared size is unusable (stale or
        // corrupt): restart from offset 0 rather than trusting it.
        runtime().block_on(async {
            let dir = unique_dir("restart");
            std::fs::create_dir_all(&dir).unwrap();
            let incomplete = dir.join("INCOMPLETE-def-x.bin");
            tokio::fs::write(&incomplete, vec![0xAAu8; 50]).await.unwrap(); // bogus 50-byte partial

            let (mut client, mut server) = tokio::io::duplex(64 * 1024);
            let dir2 = dir.clone();
            let fresh = vec![0x42u8; 20];
            let recv = tokio::spawn(async move {
                receive_to_disk(&mut server, &incomplete, &dir2, "x.bin", 20, &mut |_| {}).await
            });

            let mut off = [0u8; 8];
            client.read_exact(&mut off).await.unwrap();
            assert_eq!(u64::from_le_bytes(off), 0, "oversized partial restarts at 0");
            client.write_all(&fresh).await.unwrap();
            drop(client);

            let path = recv.await.unwrap().unwrap();
            assert_eq!(tokio::fs::read(&path).await.unwrap(), fresh, "file is the fresh bytes, not the stale partial");
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn failed_download_keeps_the_partial_for_resume() {
        // A short/aborted transfer must leave the partial on disk (under the
        // stable name) so a later attempt can resume it.
        runtime().block_on(async {
            let dir = unique_dir("keep-partial");
            std::fs::create_dir_all(&dir).unwrap();
            let (cmd_tx, cmd_rx) = unbounded_channel();
            std::mem::forget(cmd_rx);
            let ctx = Arc::new(ConnCtx {
                shares: Mutex::new(Arc::new(test_index())),
                queue: Arc::new(Mutex::new(PendingDeliveries::default())),
                downloads: Mutex::new(Downloads::default()),
            distrib: DistribState::default(),
            upload_speed: AtomicU32::new(0),
            uploads_gate: UploadGate::default(),
                uploads: Mutex::new(Uploads::default()),
                our_username: "me".into(),
                download_dir: Mutex::new(dir.clone()),
                incomplete_dir: Mutex::new(dir.clone()),
                cmd_tx,
                next_token: AtomicU32::new(1),
                excluded_phrases: Mutex::new(Vec::new()),
                upload_queue: Mutex::new(UploadQueue::new(false)),
                live: Mutex::new(LiveConfig {
                    search_filter: SearchFilter { min_files: 1, min_upload_speed: 0, max_queue_length: 0 },
                    max_results: 100,
                }),
            });
            ctx.downloads.lock().unwrap().by_token.insert(
                7,
                ActiveDownload { username: "bob".into(), filename: "Music\\got.mp3".into(), size: 100 },
            );

            let (mut client, mut server) = tokio::io::duplex(64 * 1024);
            let ctx2 = ctx.clone();
            let recv = tokio::spawn(async move {
                recv_file(&mut server, &ctx2, "bob", &mut |_| {}).await
            });

            client.write_all(&FileTransferInit { token: 7 }.to_bytes()).await.unwrap();
            let mut off = [0u8; 8];
            client.read_exact(&mut off).await.unwrap();
            client.write_all(b"partialbytes").await.unwrap(); // 12 of the promised 100
            drop(client); // abort before completion

            let outcome = recv.await.unwrap().unwrap();
            assert!(matches!(outcome, ConnOutcome::DownloadFailed { .. }), "short transfer fails");

            let partial = dir.join(incomplete_name("bob", "Music\\got.mp3", "got.mp3"));
            let meta = std::fs::metadata(&partial).expect("partial kept on disk for resume");
            assert_eq!(meta.len(), 12, "the bytes received so far are retained");
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn offers_a_shared_file_in_response_to_queue_upload() {
        // A peer asks to download one of our files; we reply with a
        // TransferRequest carrying the size, and record the pending upload.
        runtime().block_on(async {
            let (mut client, server) = tokio::io::duplex(64 * 1024);
            let ctx = test_ctx();
            let ctx_serve = ctx.clone();
            let serve = tokio::spawn(async move {
                serve_connection(server, &ctx_serve, Some("bob".into()), |_| {}).await
            });

            client
                .write_all(&QueueUpload { file: "Music\\Album\\song.mp3".into() }.to_frame())
                .await
                .unwrap();

            let reply = PeerMessage::decode(&read_one_frame(&mut client).await).unwrap();
            let PeerMessage::TransferRequest(req) = reply else { panic!("expected transfer request") };
            assert_eq!(req.direction, TransferDirection::Upload);
            assert_eq!(req.file, "Music\\Album\\song.mp3");
            assert_eq!(req.filesize, Some(4096));

            drop(client);
            serve.await.unwrap().unwrap();
            assert_eq!(ctx.uploads.lock().unwrap().by_token.len(), 1, "upload recorded by token");
        });
    }

    #[test]
    fn declines_queue_upload_for_an_unshared_file() {
        runtime().block_on(async {
            let (mut client, server) = tokio::io::duplex(64 * 1024);
            let ctx = test_ctx();
            let serve = tokio::spawn(async move {
                serve_connection(server, &ctx, Some("bob".into()), |_| {}).await
            });
            client.write_all(&QueueUpload { file: "Nope\\missing.mp3".into() }.to_frame()).await.unwrap();

            let reply = PeerMessage::decode(&read_one_frame(&mut client).await).unwrap();
            let PeerMessage::UploadDenied(denied) = reply else { panic!("expected upload denied") };
            assert_eq!(denied.file, "Nope\\missing.mp3");

            drop(client);
            serve.await.unwrap().unwrap();
        });
    }

    #[test]
    fn accepts_transfer_request_download_and_registers_the_upload() {
        // Soulseek.NET / slskd request a download with `TransferRequest{Download}`
        // rather than `QueueUpload`. We accept on the *downloader's* token and
        // register the offered upload keyed by it — without dialing out, because
        // in this flow the downloader opens the file connection to us.
        runtime().block_on(async {
            use soulseek_proto::transfer::TransferRequest;
            let (mut client, server) = tokio::io::duplex(64 * 1024);
            let ctx = test_ctx();
            let ctx_serve = ctx.clone();
            let serve = tokio::spawn(async move {
                serve_connection(server, &ctx_serve, Some("bob".into()), |_| {}).await
            });

            client
                .write_all(
                    &TransferRequest {
                        direction: TransferDirection::Download,
                        token: 99,
                        file: "Music\\Album\\song.mp3".into(),
                        filesize: None,
                    }
                    .to_frame(),
                )
                .await
                .unwrap();

            let reply = PeerMessage::decode(&read_one_frame(&mut client).await).unwrap();
            let PeerMessage::TransferResponse(resp) = reply else {
                panic!("expected transfer response")
            };
            assert_eq!(resp.token, 99, "answered on the downloader's own token");
            assert!(resp.allowed);
            assert_eq!(resp.filesize, Some(4096));

            drop(client);
            serve.await.unwrap().unwrap();
            let uploads = ctx.uploads.lock().unwrap();
            let pending = uploads.by_token.get(&99).expect("offered upload recorded by token");
            assert_eq!(pending.username, "bob");
            assert_eq!(pending.filename, "Music\\Album\\song.mp3");
            assert!(!pending.approved, "downloader connects to us; we never dial out");
        });
    }

    #[test]
    fn declines_transfer_request_download_for_an_unshared_file() {
        runtime().block_on(async {
            use soulseek_proto::transfer::TransferRequest;
            let (mut client, server) = tokio::io::duplex(64 * 1024);
            let ctx = test_ctx();
            let ctx_serve = ctx.clone();
            let serve = tokio::spawn(async move {
                serve_connection(server, &ctx_serve, Some("bob".into()), |_| {}).await
            });
            client
                .write_all(
                    &TransferRequest {
                        direction: TransferDirection::Download,
                        token: 7,
                        file: "Nope\\missing.mp3".into(),
                        filesize: None,
                    }
                    .to_frame(),
                )
                .await
                .unwrap();

            let reply = PeerMessage::decode(&read_one_frame(&mut client).await).unwrap();
            let PeerMessage::TransferResponse(resp) = reply else {
                panic!("expected transfer response")
            };
            assert_eq!(resp.token, 7);
            assert!(!resp.allowed);
            assert_eq!(resp.reason.as_deref(), Some("File not shared."));

            drop(client);
            serve.await.unwrap().unwrap();
            assert!(ctx.uploads.lock().unwrap().by_token.is_empty(), "nothing offered");
        });
    }

    #[test]
    fn serves_an_offered_upload_on_an_inbound_file_connection() {
        // The downloader opens the `F` connection to collect a file we offered:
        // peer-init (File) + the transfer token + the offset. We match the token
        // to the offered upload and stream the bytes from disk, reporting it
        // Uploaded. This is the receiving end of `TransferRequest{Download}`.
        runtime().block_on(async {
            use soulseek_proto::transfer::FileOffset;
            let dir = unique_dir("offered-upload");
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("track.mp3");
            let body = b"soulrust offered-upload body".repeat(64); // multi-chunk
            std::fs::write(&path, &body).unwrap();
            let size = body.len() as u64;

            let ctx = test_ctx();
            // Register the offered upload exactly as the TransferRequest{Download}
            // arm does: keyed by the downloader's token, not approved.
            let transfer_id = ctx.upload_queue.lock().unwrap().enqueue("bob");
            ctx.uploads.lock().unwrap().by_token.insert(
                55,
                PendingUpload {
                    username: "bob".into(),
                    filename: "Tunes\\track.mp3".into(),
                    real_path: path.clone(),
                    size,
                    approved: false,
                    transfer_id,
                },
            );

            let (mut client, server) = tokio::io::duplex(64 * 1024);
            let ctx_serve = ctx.clone();
            let serve = tokio::spawn(async move {
                serve_connection(server, &ctx_serve, None, |_| {}).await
            });

            let init =
                PeerInit { username: "bob".into(), connection_type: ConnectionType::File, token: 0 };
            client.write_all(&init.to_frame()).await.unwrap();
            client.write_all(&FileTransferInit { token: 55 }.to_bytes()).await.unwrap();
            client.write_all(&FileOffset { offset: 0 }.to_bytes()).await.unwrap();

            let mut got = Vec::new();
            client.read_to_end(&mut got).await.unwrap();

            let outcome = serve.await.unwrap().unwrap();
            let ConnOutcome::Uploaded { username, filename } = outcome else {
                panic!("expected Uploaded, got {outcome:?}");
            };
            assert_eq!(username, "bob");
            assert_eq!(filename, "Tunes\\track.mp3");
            assert_eq!(got, body, "the streamed bytes match the file on disk");
            assert!(ctx.uploads.lock().unwrap().by_token.is_empty(), "offered upload consumed");
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn pierce_file_receives_a_queued_download_after_piercing() {
        // Indirect download: the uploader was firewalled, so the server relayed
        // ConnectToPeer(F). We dial back and send PierceFirewall(connection
        // token); the uploader then sends the transfer ticket + bytes. The
        // connection token (555) and the transfer ticket (77) are independent —
        // recv_file matches on the ticket.
        runtime().block_on(async {
            let dir = unique_dir("pierce-recv");
            std::fs::create_dir_all(&dir).unwrap();
            let (cmd_tx, cmd_rx) = unbounded_channel();
            std::mem::forget(cmd_rx);
            let ctx = Arc::new(ConnCtx {
                shares: Mutex::new(Arc::new(test_index())),
                queue: Arc::new(Mutex::new(PendingDeliveries::default())),
                downloads: Mutex::new(Downloads::default()),
            distrib: DistribState::default(),
            upload_speed: AtomicU32::new(0),
            uploads_gate: UploadGate::default(),
                uploads: Mutex::new(Uploads::default()),
                our_username: "me".into(),
                download_dir: Mutex::new(dir.clone()),
                incomplete_dir: Mutex::new(dir.clone()),
                cmd_tx,
                next_token: AtomicU32::new(1),
                excluded_phrases: Mutex::new(Vec::new()),
                upload_queue: Mutex::new(UploadQueue::new(false)),
                live: Mutex::new(LiveConfig {
                    search_filter: SearchFilter { min_files: 1, min_upload_speed: 0, max_queue_length: 0 },
                    max_results: 100,
                }),
            });
            let payload = b"firewalled upload payload bytes".repeat(4);
            let size = payload.len() as u64;
            ctx.downloads.lock().unwrap().by_token.insert(
                77,
                ActiveDownload { username: "up".into(), filename: "Dir\\f.mp3".into(), size },
            );

            let (mut client, mut server) = tokio::io::duplex(64 * 1024);
            let ctx2 = ctx.clone();
            let recv = tokio::spawn(async move {
                pierce_and_recv_file(&mut server, 555, &ctx2, "up", &mut |_| {}).await
            });

            // We pierce first, carrying the connection token from ConnectToPeer.
            let pierce_frame = read_one_frame(&mut client).await;
            let PeerInitMessage::PierceFirewall(p) = PeerInitMessage::decode(&pierce_frame).unwrap()
            else {
                panic!("expected a pierce-firewall")
            };
            assert_eq!(p.token, 555, "pierce carries the connection token, not the ticket");

            // The uploader then sends the transfer ticket; we send the offset; it streams.
            client.write_all(&FileTransferInit { token: 77 }.to_bytes()).await.unwrap();
            let mut off = [0u8; 8];
            client.read_exact(&mut off).await.unwrap();
            client.write_all(&payload).await.unwrap();
            drop(client);

            let outcome = recv.await.unwrap().unwrap();
            let ConnOutcome::Downloaded { filename, .. } = outcome else {
                panic!("expected Downloaded, got {outcome:?}");
            };
            assert_eq!(filename, "Dir\\f.mp3");
            assert_eq!(tokio::fs::read(dir.join("f.mp3")).await.unwrap(), payload);
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn pierce_file_serves_an_offered_upload_after_piercing() {
        // Indirect upload: the downloader was firewalled (it offered via
        // TransferRequest{Download} then couldn't open the F connection), so the
        // server relayed ConnectToPeer(F). We dial back, pierce, and serve the
        // file we offered — recv_file matches the ticket to the offered upload.
        runtime().block_on(async {
            use soulseek_proto::transfer::FileOffset;
            let dir = unique_dir("pierce-serve");
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("track.mp3");
            let body = b"pierced upload body bytes".repeat(8);
            std::fs::write(&path, &body).unwrap();
            let size = body.len() as u64;

            let ctx = test_ctx();
            let transfer_id = ctx.upload_queue.lock().unwrap().enqueue("dl");
            ctx.uploads.lock().unwrap().by_token.insert(
                88,
                PendingUpload {
                    username: "dl".into(),
                    filename: "Tunes\\track.mp3".into(),
                    real_path: path.clone(),
                    size,
                    approved: false,
                    transfer_id,
                },
            );

            let (mut client, mut server) = tokio::io::duplex(64 * 1024);
            let ctx2 = ctx.clone();
            let serve = tokio::spawn(async move {
                pierce_and_recv_file(&mut server, 42, &ctx2, "dl", &mut |_| {}).await
            });

            let pierce_frame = read_one_frame(&mut client).await;
            let PeerInitMessage::PierceFirewall(p) = PeerInitMessage::decode(&pierce_frame).unwrap()
            else {
                panic!("expected a pierce-firewall")
            };
            assert_eq!(p.token, 42);

            // The downloader sends the transfer ticket + offset; we stream the file.
            client.write_all(&FileTransferInit { token: 88 }.to_bytes()).await.unwrap();
            client.write_all(&FileOffset { offset: 0 }.to_bytes()).await.unwrap();
            let mut got = Vec::new();
            client.read_to_end(&mut got).await.unwrap();

            let outcome = serve.await.unwrap().unwrap();
            let ConnOutcome::Uploaded { filename, .. } = outcome else {
                panic!("expected Uploaded, got {outcome:?}");
            };
            assert_eq!(filename, "Tunes\\track.mp3");
            assert_eq!(got, body);
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn download_basename_truncates_long_names_preserving_extension() {
        // Short name: unchanged.
        assert_eq!(download_basename("Music\\Album\\song.mp3"), "song.mp3");
        // Over the limit: result fits MAX_BASENAME_BYTES and keeps the extension.
        let long = format!("Music\\{}.mp3", "a".repeat(400));
        let basename = download_basename(&long);
        assert!(basename.len() <= MAX_BASENAME_BYTES);
        assert!(basename.ends_with(".mp3"), "extension preserved");
        // Multi-byte chars are truncated on a boundary (no panic, still valid UTF-8).
        let multibyte = format!("{}.mp3", "片".repeat(200));
        let basename = download_basename(&multibyte);
        assert!(basename.len() <= MAX_BASENAME_BYTES);
        assert!(basename.ends_with(".mp3"));
    }

    #[test]
    fn unique_download_path_disambiguates_collisions() {
        runtime().block_on(async {
            let dir = std::env::temp_dir().join(format!("soulrust-uniq-{}", std::process::id()));
            let _ = tokio::fs::create_dir_all(&dir).await;
            // No existing file: the plain name.
            let p0 = unique_download_path(&dir, "track.mp3").await;
            assert_eq!(p0, dir.join("track.mp3"));
            // Create it, then the next call disambiguates.
            tokio::fs::write(&p0, b"x").await.unwrap();
            let p1 = unique_download_path(&dir, "track.mp3").await;
            assert_eq!(p1, dir.join("track (1).mp3"));
            let _ = tokio::fs::remove_dir_all(&dir).await;
        });
    }

    #[test]
    fn recv_file_rejects_a_token_from_the_wrong_peer() {
        // A download negotiated with "alice" must not be collected by a different
        // peer that guesses the (uploader-chosen) token. The entry is preserved.
        runtime().block_on(async {
            let dir = std::env::temp_dir().join(format!("soulrust-wrongpeer-{}", std::process::id()));
            let _ = std::fs::create_dir_all(&dir);
            let (cmd_tx, cmd_rx) = unbounded_channel();
            std::mem::forget(cmd_rx);
            let ctx = Arc::new(ConnCtx {
                shares: Mutex::new(Arc::new(test_index())),
                queue: Arc::new(Mutex::new(PendingDeliveries::default())),
                downloads: Mutex::new(Downloads::default()),
            distrib: DistribState::default(),
            upload_speed: AtomicU32::new(0),
            uploads_gate: UploadGate::default(),
                uploads: Mutex::new(Uploads::default()),
                our_username: "me".into(),
                download_dir: Mutex::new(dir.clone()),
                incomplete_dir: Mutex::new(dir.clone()),
                cmd_tx,
                next_token: AtomicU32::new(1),
                excluded_phrases: Mutex::new(Vec::new()),
                upload_queue: Mutex::new(UploadQueue::new(false)),
                live: Mutex::new(LiveConfig {
                    search_filter: SearchFilter { min_files: 1, min_upload_speed: 0, max_queue_length: 0 },
                    max_results: 100,
                }),
            });
            ctx.downloads.lock().unwrap().by_token.insert(
                42,
                ActiveDownload { username: "alice".into(), filename: "a.mp3".into(), size: 5 },
            );

            let (mut client, server) = tokio::io::duplex(1024);
            let ctx_serve = ctx.clone();
            let serve = tokio::spawn(async move {
                serve_connection(server, &ctx_serve, None, |_| {}).await
            });
            // Mallory opens an F connection and pierces alice's token.
            let init = PeerInit { username: "mallory".into(), connection_type: ConnectionType::File, token: 0 };
            client.write_all(&init.to_frame()).await.unwrap();
            client.write_all(&FileTransferInit { token: 42 }.to_bytes()).await.unwrap();
            drop(client);

            let outcome = serve.await.unwrap().unwrap();
            assert!(matches!(outcome, ConnOutcome::Done), "wrong-peer token must be dropped");
            assert!(
                ctx.downloads.lock().unwrap().by_token.contains_key(&42),
                "alice's download is preserved for her real connection"
            );
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn accepted_download_uses_our_recorded_size_not_the_uploaders() {
        // A malicious uploader sends filesize=0 to truncate; we must use the size
        // we recorded when queueing (5), not theirs.
        runtime().block_on(async {
            use soulseek_proto::transfer::TransferRequest;
            let (mut client, server) = tokio::io::duplex(64 * 1024);
            let ctx = test_ctx();
            ctx.downloads.lock().unwrap().pending.insert(("bob".into(), "f.mp3".into()), 5);
            let ctx_serve = ctx.clone();
            let serve = tokio::spawn(async move {
                serve_connection(server, &ctx_serve, Some("bob".into()), |_| {}).await
            });
            client
                .write_all(
                    &TransferRequest {
                        direction: TransferDirection::Upload,
                        token: 9,
                        file: "f.mp3".into(),
                        filesize: Some(0), // understated
                    }
                    .to_frame(),
                )
                .await
                .unwrap();
            let _ = PeerMessage::decode(&read_one_frame(&mut client).await).unwrap();
            drop(client);
            serve.await.unwrap().unwrap();
            assert_eq!(
                ctx.downloads.lock().unwrap().by_token.get(&9).map(|d| d.size),
                Some(5),
                "our recorded size is used, not the uploader's filesize"
            );
        });
    }

    #[test]
    fn upload_denied_fails_the_queued_download() {
        // We queued a download; the uploader refuses with a hard reason. We must
        // fail fast (emit DownloadFailed) and clear the pending entry instead of
        // hanging forever waiting for a TransferRequest that never comes.
        runtime().block_on(async {
            use soulseek_proto::transfer::UploadDenied;
            let (mut client, server) = tokio::io::duplex(64 * 1024);
            let ctx = test_ctx();
            ctx.downloads.lock().unwrap().pending.insert(("bob".into(), "f.mp3".into()), 5);
            let ctx_serve = ctx.clone();
            let serve = tokio::spawn(async move {
                serve_connection(server, &ctx_serve, Some("bob".into()), |_| {}).await
            });
            client
                .write_all(
                    &UploadDenied { file: "f.mp3".into(), reason: "File not shared".into() }
                        .to_frame(),
                )
                .await
                .unwrap();
            drop(client);
            let outcome = serve.await.unwrap().unwrap();
            let ConnOutcome::DownloadFailed { username, filename, reason } = outcome else {
                panic!("expected DownloadFailed, got {outcome:?}");
            };
            assert_eq!((username.as_str(), filename.as_str()), ("bob", "f.mp3"));
            assert_eq!(reason, "File not shared");
            assert!(ctx.downloads.lock().unwrap().pending.is_empty(), "pending entry cleared");
        });
    }

    #[test]
    fn upload_denied_queued_reason_keeps_waiting() {
        // A reason of "Queued" (optionally with a trailing '.') means remotely
        // queued, not a rejection: the download stays pending for the eventual
        // TransferRequest rather than being failed.
        runtime().block_on(async {
            use soulseek_proto::transfer::UploadDenied;
            let (mut client, server) = tokio::io::duplex(64 * 1024);
            let ctx = test_ctx();
            ctx.downloads.lock().unwrap().pending.insert(("bob".into(), "f.mp3".into()), 5);
            let ctx_serve = ctx.clone();
            let serve = tokio::spawn(async move {
                serve_connection(server, &ctx_serve, Some("bob".into()), |_| {}).await
            });
            client
                .write_all(&UploadDenied { file: "f.mp3".into(), reason: "Queued.".into() }.to_frame())
                .await
                .unwrap();
            drop(client);
            let outcome = serve.await.unwrap().unwrap();
            assert!(matches!(outcome, ConnOutcome::Done), "queued is not a failure");
            assert!(
                ctx.downloads
                    .lock()
                    .unwrap()
                    .pending
                    .contains_key(&("bob".to_string(), "f.mp3".to_string())),
                "download stays pending while remotely queued"
            );
        });
    }

    #[test]
    fn upload_failed_fails_the_queued_download() {
        // The uploader reports a file we queued failed mid-transfer: surface it.
        runtime().block_on(async {
            use soulseek_proto::transfer::UploadFailed;
            let (mut client, server) = tokio::io::duplex(64 * 1024);
            let ctx = test_ctx();
            ctx.downloads.lock().unwrap().pending.insert(("bob".into(), "f.mp3".into()), 5);
            let ctx_serve = ctx.clone();
            let serve = tokio::spawn(async move {
                serve_connection(server, &ctx_serve, Some("bob".into()), |_| {}).await
            });
            client.write_all(&UploadFailed { file: "f.mp3".into() }.to_frame()).await.unwrap();
            drop(client);
            let outcome = serve.await.unwrap().unwrap();
            let ConnOutcome::DownloadFailed { filename, .. } = outcome else {
                panic!("expected DownloadFailed, got {outcome:?}");
            };
            assert_eq!(filename, "f.mp3");
            assert!(ctx.downloads.lock().unwrap().pending.is_empty());
        });
    }

    #[test]
    fn slot_advertisement_reflects_pending_uploads() {
        // No pending uploads -> a free slot and empty queue; with work queued we
        // report it instead of the old hardcoded "always free, zero queue".
        assert_eq!(slot_advertisement(0), (true, 0));
        assert_eq!(slot_advertisement(3), (false, 3));
    }

    #[test]
    fn serve_distrib_responds_to_a_distributed_search() {
        // A DistribSearch on a D connection is fed into the responder path via a
        // PeerCommand::IncomingSearch (same as a server search).
        use soulseek_proto::distributed::{DistribSearch, SEARCH_IDENTIFIER};
        runtime().block_on(async {
            let (mut client, mut server) = tokio::io::duplex(64 * 1024);
            let (cmd_tx, mut cmd_rx) = unbounded_channel();
            let ctx = Arc::new(ConnCtx {
                shares: Mutex::new(Arc::new(test_index())),
                queue: Arc::new(Mutex::new(PendingDeliveries::default())),
                downloads: Mutex::new(Downloads::default()),
            distrib: DistribState::default(),
            upload_speed: AtomicU32::new(0),
            uploads_gate: UploadGate::default(),
                uploads: Mutex::new(Uploads::default()),
                our_username: "me".into(),
                download_dir: Mutex::new(std::env::temp_dir()),
                incomplete_dir: Mutex::new(std::env::temp_dir()),
                cmd_tx,
                next_token: AtomicU32::new(1),
                excluded_phrases: Mutex::new(Vec::new()),
                upload_queue: Mutex::new(UploadQueue::new(false)),
                live: Mutex::new(LiveConfig {
                    search_filter: SearchFilter { min_files: 1, min_upload_speed: 0, max_queue_length: 0 },
                    max_results: 100,
                }),
            });
            let serve = tokio::spawn(async move {
                serve_distrib(&mut server, &ctx, "parent", &mut |_| {}).await
            });

            let search = DistribSearch {
                identifier: SEARCH_IDENTIFIER,
                username: "searcher".into(),
                token: 7,
                query: "jazz".into(),
            };
            client.write_all(&search.to_frame()).await.unwrap();
            drop(client);
            serve.await.unwrap().unwrap();

            match cmd_rx.try_recv() {
                Ok(PeerCommand::IncomingSearch { username, token, query }) => {
                    assert_eq!(username, "searcher");
                    assert_eq!(token, 7);
                    assert_eq!(query, "jazz");
                }
                _ => panic!("expected an IncomingSearch from the distributed search"),
            }
        });
    }

    #[test]
    fn parent_branch_level_sets_our_level_and_pushes_to_children() {
        runtime().block_on(async {
            let ctx = test_ctx();
            // A registered child whose forwarded frames we can read.
            let (tx, mut rx) = unbounded_channel::<Vec<u8>>();
            ctx.add_child(tx);

            // Feed the parent's BranchLevel(4) then end the connection.
            let (mut parent, mut server) = tokio::io::duplex(64 * 1024);
            parent.write_all(&DistribBranchLevel { level: 4 }.to_frame()).await.unwrap();
            drop(parent);
            serve_distrib_loop(&mut server, &ctx, "parent", &mut |_| {}).await.unwrap();

            // Our level is the parent's + 1, and the child was pushed the update
            // as a ready-to-send (length-prefixed) frame.
            assert_eq!(ctx.branch_snapshot().level, 5);
            let frame = rx.recv().await.unwrap();
            assert_eq!(frame, DistribBranchLevel { level: 5 }.to_frame());
        });
    }

    #[test]
    fn max_children_scales_with_upload_speed_and_server_limits() {
        // Below the server's min speed, or before it sends a ratio: no children.
        assert_eq!(compute_max_children(0, 0, 0), 0);
        assert_eq!(compute_max_children(500, 1000, 100), 0, "too slow");
        // Fast enough: min(speed / ratio / 100, 10).
        assert_eq!(compute_max_children(100_000, 0, 100), 10, "capped at 10");
        assert_eq!(compute_max_children(30_000, 0, 100), 3);
        assert_eq!(compute_max_children(1000, 1000, 100), 0, "1000/100/100 = 0");
    }

    #[test]
    fn config_change_reapplies_folders_and_dirs_live() {
        use crate::config::{AppContext, Config};
        struct W;
        impl Clone for W {
            fn clone(&self) -> Self {
                W
            }
        }
        impl traits::core::Writer for W {
            fn write<M: traits::core::Message, H: traits::core::Handler, F: FnOnce(&mut [u8])>(
                &self,
                _size: usize,
                _callback: F,
            ) {
            }
        }
        let ctx = AppContext::new(Config::default(), std::path::PathBuf::from("/tmp/x.yaml"));
        let mut pn = PeerNet::new(&ctx, &W);

        let mut changed = Config::default();
        changed.sharing.folders = vec![std::env::temp_dir().join("sr-share").display().to_string()];
        changed.sharing.download_dir = std::env::temp_dir().join("sr-dl").display().to_string();
        traits::core::Handle::<ConfigChanged>::handle(
            &mut pn,
            &ConfigChanged { config: soulrust_proto::MessageField::some(crate::config::config_to_proto(&changed)), ..Default::default() },
            &W,
        );

        // The config change becomes an ApplyConfig carrying the new folders and
        // the resolved download dir — applied without a restart.
        match pn.cmd_rx.as_mut().unwrap().try_recv().expect("ApplyConfig queued") {
            PeerCommand::ApplyConfig { folders, download_dir, .. } => {
                assert_eq!(folders, vec![PathBuf::from(&changed.sharing.folders[0])]);
                assert_eq!(download_dir, changed.sharing.download_path());
            }
            _ => panic!("expected ApplyConfig"),
        }
    }

    #[test]
    fn upload_gate_caps_concurrency_at_the_slot_count() {
        let gate = UploadGate::default();
        gate.slots.store(2, Ordering::Relaxed);
        let job = || UploadJob {
            ip: "1.2.3.4".into(),
            port: 1,
            token: 0,
            peer: "p".into(),
            filename: "f".into(),
            real_path: std::path::PathBuf::from("/x"),
            size: 1,
        };
        for _ in 0..3 {
            gate.waiting.lock().unwrap().push_back(job());
        }
        // Only two of the three can be claimed at once.
        assert!(gate.try_claim().is_some());
        assert!(gate.try_claim().is_some());
        assert!(gate.try_claim().is_none(), "at capacity");
        // Freeing a slot lets the third start; then we're empty.
        gate.release();
        assert!(gate.try_claim().is_some());
        assert!(gate.try_claim().is_none(), "nothing left waiting");
    }

    #[test]
    fn upload_gate_zero_slots_means_unlimited() {
        let gate = UploadGate::default(); // slots = 0
        for _ in 0..5 {
            gate.waiting.lock().unwrap().push_back(UploadJob {
                ip: "1.2.3.4".into(),
                port: 1,
                token: 0,
                peer: "p".into(),
                filename: "f".into(),
                real_path: std::path::PathBuf::from("/x"),
                size: 1,
            });
        }
        for _ in 0..5 {
            assert!(gate.try_claim().is_some());
        }
    }

    #[test]
    fn upload_speed_is_a_rolling_average() {
        let ctx = test_ctx();
        assert_eq!(ctx.upload_speed.load(Ordering::Relaxed), 0);
        ctx.record_upload_speed(1000); // first sample seeds the average
        assert_eq!(ctx.upload_speed.load(Ordering::Relaxed), 1000);
        ctx.record_upload_speed(2000); // 1000*0.7 + 2000*0.3 = 1300
        assert_eq!(ctx.upload_speed.load(Ordering::Relaxed), 1300);
    }

    #[test]
    fn forwards_branch_info_and_searches_to_a_child() {
        // A child that connects to us is told our branch position and then has
        // searches relayed down to it.
        use soulseek_proto::distributed::{DistribSearch, SEARCH_IDENTIFIER};
        runtime().block_on(async {
            let ctx = test_ctx();
            {
                let mut branch = ctx.distrib.branch.lock().unwrap();
                branch.level = 2;
                branch.root = "alice".into();
            }
            // Enough capacity to accept a child: speed/ratio give max_children >= 1.
            ctx.upload_speed.store(100, Ordering::Relaxed);
            ctx.distrib.speed_ratio.store(1, Ordering::Relaxed);
            let (mut child, mut server) = tokio::io::duplex(64 * 1024);
            let ctx2 = ctx.clone();
            let task =
                tokio::spawn(async move { serve_child(&mut server, &ctx2, "kid", &mut |_| {}).await });

            // On connect it receives our branch level, then root.
            let frame = read_one_frame(&mut child).await;
            assert_eq!(
                DistributedMessage::decode(&frame).unwrap(),
                DistributedMessage::BranchLevel(DistribBranchLevel { level: 2 })
            );
            let frame = read_one_frame(&mut child).await;
            assert_eq!(
                DistributedMessage::decode(&frame).unwrap(),
                DistributedMessage::BranchRoot(DistribBranchRoot { root_username: "alice".into() })
            );

            // Registered as a child; a forwarded search reaches it verbatim.
            assert_eq!(ctx.child_count(), 1);
            let search = DistribSearch {
                identifier: SEARCH_IDENTIFIER,
                username: "carol".into(),
                token: 7,
                query: "wish".into(),
            };
            assert_eq!(ctx.forward_to_children(&search.to_frame()), 1);
            let frame = read_one_frame(&mut child).await;
            assert_eq!(DistributedMessage::decode(&frame).unwrap(), DistributedMessage::Search(search));

            // Dropping the registration ends the child's connection task.
            ctx.distrib.children.lock().unwrap().clear();
            let _ = task.await;
            assert_eq!(ctx.child_count(), 0);
        });
    }

    #[test]
    fn pierce_distrib_relays_searches_after_piercing() {
        // Indirect distributed connect: the server relayed ConnectToPeer(D). We
        // dial back, send PierceFirewall(token), then relay the peer's
        // DistribSearch into the responder path — just like a direct D connection.
        use soulseek_proto::distributed::{DistribSearch, SEARCH_IDENTIFIER};
        runtime().block_on(async {
            let (mut client, mut server) = tokio::io::duplex(64 * 1024);
            let (cmd_tx, mut cmd_rx) = unbounded_channel();
            let ctx = Arc::new(ConnCtx {
                shares: Mutex::new(Arc::new(test_index())),
                queue: Arc::new(Mutex::new(PendingDeliveries::default())),
                downloads: Mutex::new(Downloads::default()),
            distrib: DistribState::default(),
            upload_speed: AtomicU32::new(0),
            uploads_gate: UploadGate::default(),
                uploads: Mutex::new(Uploads::default()),
                our_username: "me".into(),
                download_dir: Mutex::new(std::env::temp_dir()),
                incomplete_dir: Mutex::new(std::env::temp_dir()),
                cmd_tx,
                next_token: AtomicU32::new(1),
                excluded_phrases: Mutex::new(Vec::new()),
                upload_queue: Mutex::new(UploadQueue::new(false)),
                live: Mutex::new(LiveConfig {
                    search_filter: SearchFilter { min_files: 1, min_upload_speed: 0, max_queue_length: 0 },
                    max_results: 100,
                }),
            });
            let serve = tokio::spawn(async move {
                pierce_and_serve_distrib(&mut server, 321, &ctx, "eve", &mut |_| {}).await
            });

            // We pierce first, carrying the connection token from ConnectToPeer.
            let pierce_frame = read_one_frame(&mut client).await;
            let PeerInitMessage::PierceFirewall(p) = PeerInitMessage::decode(&pierce_frame).unwrap()
            else {
                panic!("expected a pierce-firewall")
            };
            assert_eq!(p.token, 321);

            // The peer then sends a distributed search; we relay it onward.
            let search = DistribSearch {
                identifier: SEARCH_IDENTIFIER,
                username: "searcher".into(),
                token: 7,
                query: "jazz".into(),
            };
            client.write_all(&search.to_frame()).await.unwrap();
            drop(client);
            serve.await.unwrap().unwrap();

            match cmd_rx.try_recv() {
                Ok(PeerCommand::IncomingSearch { username, token, query }) => {
                    assert_eq!(username, "searcher");
                    assert_eq!(token, 7);
                    assert_eq!(query, "jazz");
                }
                _ => panic!("expected an IncomingSearch from the pierced distributed search"),
            }
        });
    }

    #[test]
    fn approved_upload_triggers_a_start_upload_command() {
        // When the downloader approves an offer, we mark it and ask the reactor
        // to resolve the address and open the file connection.
        use soulseek_proto::transfer::TransferResponse;
        runtime().block_on(async {
            let (mut client, server) = tokio::io::duplex(64 * 1024);
            let (cmd_tx, mut cmd_rx) = unbounded_channel();
            let ctx = Arc::new(ConnCtx {
                shares: Mutex::new(Arc::new(test_index())),
                queue: Arc::new(Mutex::new(PendingDeliveries::default())),
                downloads: Mutex::new(Downloads::default()),
            distrib: DistribState::default(),
            upload_speed: AtomicU32::new(0),
            uploads_gate: UploadGate::default(),
                uploads: Mutex::new(Uploads::default()),
                our_username: "me".into(),
                download_dir: Mutex::new(std::env::temp_dir()),
                incomplete_dir: Mutex::new(std::env::temp_dir()),
                cmd_tx,
                next_token: AtomicU32::new(1),
                excluded_phrases: Mutex::new(Vec::new()),
                upload_queue: Mutex::new(UploadQueue::new(false)),
                live: Mutex::new(LiveConfig {
                    search_filter: SearchFilter { min_files: 1, min_upload_speed: 0, max_queue_length: 0 },
                    max_results: 100,
                }),
            });
            let tid = ctx.upload_queue.lock().unwrap().enqueue("bob");
            ctx.uploads.lock().unwrap().by_token.insert(
                5,
                PendingUpload {
                    username: "bob".into(),
                    filename: "Music\\Album\\song.mp3".into(),
                    real_path: "/tmp/song.mp3".into(),
                    size: 4096,
                    approved: false,
                    transfer_id: tid,
                },
            );

            let serve = tokio::spawn(async move {
                serve_connection(server, &ctx, Some("bob".into()), |_| {}).await
            });
            client
                .write_all(
                    &TransferResponse { token: 5, allowed: true, filesize: None, reason: None }
                        .to_frame(),
                )
                .await
                .unwrap();
            drop(client);
            serve.await.unwrap().unwrap();

            match cmd_rx.try_recv() {
                Ok(PeerCommand::StartUpload { username }) => assert_eq!(username, "bob"),
                other => panic!("expected StartUpload, got {:?}", other.is_ok()),
            }
        });
    }

    #[test]
    fn answers_place_in_queue_with_the_real_position() {
        // A PlaceInQueueRequest gets the FIFO position of the matching offered
        // upload — here the second of two queued for this peer.
        runtime().block_on(async {
            let (mut client, server) = tokio::io::duplex(64 * 1024);
            let ctx = test_ctx();
            let _first = ctx.upload_queue.lock().unwrap().enqueue("bob");
            let second = ctx.upload_queue.lock().unwrap().enqueue("bob");
            ctx.uploads.lock().unwrap().by_token.insert(
                2,
                PendingUpload {
                    username: "bob".into(),
                    filename: "Music\\b.mp3".into(),
                    real_path: "/tmp/b.mp3".into(),
                    size: 10,
                    approved: false,
                    transfer_id: second,
                },
            );
            let ctx_serve = ctx.clone();
            let serve = tokio::spawn(async move {
                serve_connection(server, &ctx_serve, Some("bob".into()), |_| {}).await
            });

            client
                .write_all(&PlaceInQueueRequest { file: "Music\\b.mp3".into() }.to_frame())
                .await
                .unwrap();
            let reply = PeerMessage::decode(&read_one_frame(&mut client).await).unwrap();
            let PeerMessage::PlaceInQueueResponse(resp) = reply else {
                panic!("expected a place-in-queue response");
            };
            assert_eq!(resp.filename, "Music\\b.mp3");
            assert_eq!(resp.place, 2, "second in the queue");
            drop(client);
            serve.await.unwrap().unwrap();
        });
    }

    #[test]
    fn place_in_queue_is_zero_for_an_unoffered_file() {
        // No matching offered upload -> position 0 ("not queued").
        runtime().block_on(async {
            let (mut client, server) = tokio::io::duplex(64 * 1024);
            let ctx = test_ctx();
            let ctx_serve = ctx.clone();
            let serve = tokio::spawn(async move {
                serve_connection(server, &ctx_serve, Some("bob".into()), |_| {}).await
            });

            client
                .write_all(&PlaceInQueueRequest { file: "Music\\never-offered.mp3".into() }.to_frame())
                .await
                .unwrap();
            let reply = PeerMessage::decode(&read_one_frame(&mut client).await).unwrap();
            let PeerMessage::PlaceInQueueResponse(resp) = reply else {
                panic!("expected a place-in-queue response");
            };
            assert_eq!(resp.place, 0);
            drop(client);
            serve.await.unwrap().unwrap();
        });
    }

    #[test]
    fn queue_upload_enqueues_and_rejection_dequeues() {
        // QueueUpload for a shared file enqueues it (so place-in-queue is real);
        // a downloader's rejection removes both the offer and its queue slot.
        use soulseek_proto::transfer::TransferResponse;
        runtime().block_on(async {
            let (mut client, server) = tokio::io::duplex(64 * 1024);
            let ctx = test_ctx();
            let ctx_serve = ctx.clone();
            let serve = tokio::spawn(async move {
                serve_connection(server, &ctx_serve, Some("bob".into()), |_| {}).await
            });

            // Offer one of our shared files; the queue now holds it.
            client
                .write_all(&QueueUpload { file: "Music\\Album\\song.mp3".into() }.to_frame())
                .await
                .unwrap();
            let reply = PeerMessage::decode(&read_one_frame(&mut client).await).unwrap();
            let PeerMessage::TransferRequest(req) = reply else { panic!("expected transfer request") };
            assert_eq!(ctx.upload_queue.lock().unwrap().len(), 1, "offer is queued");

            // Reject it; the queue slot is released.
            client
                .write_all(
                    &TransferResponse {
                        token: req.token,
                        allowed: false,
                        filesize: None,
                        reason: Some("not now".into()),
                    }
                    .to_frame(),
                )
                .await
                .unwrap();
            drop(client);
            serve.await.unwrap().unwrap();
            assert_eq!(ctx.upload_queue.lock().unwrap().len(), 0, "rejection dequeues");
        });
    }

    #[test]
    fn place_in_queue_response_is_forwarded_to_the_ui() {
        // As the downloader, an inbound PlaceInQueueResponse is relayed to the
        // reactor (which turns it into a DownloadQueuePosition for the UI).
        runtime().block_on(async {
            let (mut client, server) = tokio::io::duplex(64 * 1024);
            let (cmd_tx, mut cmd_rx) = unbounded_channel();
            let ctx = Arc::new(ConnCtx {
                shares: Mutex::new(Arc::new(test_index())),
                queue: Arc::new(Mutex::new(PendingDeliveries::default())),
                downloads: Mutex::new(Downloads::default()),
            distrib: DistribState::default(),
            upload_speed: AtomicU32::new(0),
            uploads_gate: UploadGate::default(),
                uploads: Mutex::new(Uploads::default()),
                our_username: "me".into(),
                download_dir: Mutex::new(std::env::temp_dir()),
                incomplete_dir: Mutex::new(std::env::temp_dir()),
                cmd_tx,
                next_token: AtomicU32::new(1),
                excluded_phrases: Mutex::new(Vec::new()),
                upload_queue: Mutex::new(UploadQueue::new(false)),
                live: Mutex::new(LiveConfig {
                    search_filter: SearchFilter { min_files: 1, min_upload_speed: 0, max_queue_length: 0 },
                    max_results: 100,
                }),
            });
            let ctx_serve = ctx.clone();
            let serve = tokio::spawn(async move {
                serve_connection(server, &ctx_serve, Some("alice".into()), |_| {}).await
            });

            client
                .write_all(
                    &PlaceInQueueResponse { filename: "Music\\x.mp3".into(), place: 3 }.to_frame(),
                )
                .await
                .unwrap();
            drop(client);
            serve.await.unwrap().unwrap();

            match cmd_rx.try_recv() {
                Ok(PeerCommand::QueuePosition { username, filename, place }) => {
                    assert_eq!((username.as_str(), filename.as_str(), place), ("alice", "Music\\x.mp3", 3));
                }
                other => panic!("expected QueuePosition, got ok={}", other.is_ok()),
            }
        });
    }

    /// A ConnCtx with a live command receiver and a given search filter, for the
    /// inbound-search-result tests.
    fn ctx_with_filter(filter: SearchFilter) -> (Arc<ConnCtx>, UnboundedReceiver<PeerCommand>) {
        let (cmd_tx, cmd_rx) = unbounded_channel();
        let ctx = Arc::new(ConnCtx {
            shares: Mutex::new(Arc::new(test_index())),
            queue: Arc::new(Mutex::new(PendingDeliveries::default())),
            downloads: Mutex::new(Downloads::default()),
            distrib: DistribState::default(),
            upload_speed: AtomicU32::new(0),
            uploads_gate: UploadGate::default(),
            uploads: Mutex::new(Uploads::default()),
            our_username: "me".into(),
            download_dir: Mutex::new(std::env::temp_dir()),
            incomplete_dir: Mutex::new(std::env::temp_dir()),
            cmd_tx,
            next_token: AtomicU32::new(1),
            excluded_phrases: Mutex::new(Vec::new()),
            upload_queue: Mutex::new(UploadQueue::new(false)),
            live: Mutex::new(LiveConfig { search_filter: filter, max_results: 100 }),
        });
        (ctx, cmd_rx)
    }

    fn inbound_response(token: u32, files: usize, speed: u32, queue: u32) -> FileSearchResponse {
        use soulseek_proto::peer_message::SharedFile;
        FileSearchResponse {
            username: "peer".into(),
            token,
            files: (0..files)
                .map(|i| SharedFile { name: format!("Music\\hit{i}.mp3"), size: 100, ..Default::default() })
                .collect(),
            free_slots: true,
            upload_speed: speed,
            in_queue: queue,
            private_files: Vec::new(),
        }
    }

    #[test]
    fn inbound_search_result_passing_filter_is_forwarded() {
        // As the searcher, a peer's response that clears the filter is relayed to
        // the reactor (which turns it into a SearchResultReceived for the UI).
        runtime().block_on(async {
            let (mut client, server) = tokio::io::duplex(64 * 1024);
            let (ctx, mut cmd_rx) =
                ctx_with_filter(SearchFilter { min_files: 1, min_upload_speed: 0, max_queue_length: 0 });
            let ctx_serve = ctx.clone();
            let serve = tokio::spawn(async move {
                serve_connection(server, &ctx_serve, Some("peer".into()), |_| {}).await
            });

            client.write_all(&inbound_response(42, 1, 500, 0).to_frame()).await.unwrap();
            drop(client);
            serve.await.unwrap().unwrap();

            match cmd_rx.try_recv() {
                Ok(PeerCommand::SearchResult { token, username, upload_speed, files, .. }) => {
                    assert_eq!(token, 42);
                    assert_eq!(username, "peer");
                    assert_eq!(upload_speed, 500);
                    assert_eq!(files.len(), 1);
                    assert_eq!(files[0].name, "Music\\hit0.mp3");
                }
                other => panic!("expected SearchResult, got ok={}", other.is_ok()),
            }
        });
    }

    #[test]
    fn search_filter_change_applies_live() {
        // Start permissive, then tighten the filter at runtime exactly as a
        // ConfigChanged does (ApplyConfig replaces ctx.live). The reactor reads
        // ctx.live per response, so the new threshold takes effect without a
        // restart and a now-too-slow result is dropped.
        runtime().block_on(async {
            let (mut client, server) = tokio::io::duplex(64 * 1024);
            let (ctx, mut cmd_rx) =
                ctx_with_filter(SearchFilter { min_files: 1, min_upload_speed: 0, max_queue_length: 0 });
            // Live config update (what PeerCommand::ApplyConfig applies).
            *ctx.live.lock().unwrap() = LiveConfig {
                search_filter: SearchFilter { min_files: 1, min_upload_speed: 1_000, max_queue_length: 0 },
                max_results: 100,
            };
            let ctx_serve = ctx.clone();
            let serve = tokio::spawn(async move {
                serve_connection(server, &ctx_serve, Some("peer".into()), |_| {}).await
            });

            client.write_all(&inbound_response(7, 1, 10, 0).to_frame()).await.unwrap(); // 10 < 1000
            drop(client);
            serve.await.unwrap().unwrap();
            assert!(cmd_rx.try_recv().is_err(), "live-tightened filter drops the slow result");
        });
    }

    #[test]
    fn inbound_search_result_failing_filter_is_dropped() {
        // A response below the minimum upload speed never reaches the reactor.
        runtime().block_on(async {
            let (mut client, server) = tokio::io::duplex(64 * 1024);
            let (ctx, mut cmd_rx) = ctx_with_filter(SearchFilter {
                min_files: 1,
                min_upload_speed: 1_000,
                max_queue_length: 0,
            });
            let ctx_serve = ctx.clone();
            let serve = tokio::spawn(async move {
                serve_connection(server, &ctx_serve, Some("peer".into()), |_| {}).await
            });

            client.write_all(&inbound_response(7, 3, 10, 0).to_frame()).await.unwrap();
            drop(client);
            serve.await.unwrap().unwrap();

            assert!(cmd_rx.try_recv().is_err(), "slow peer's result is filtered out");
        });
    }
}
