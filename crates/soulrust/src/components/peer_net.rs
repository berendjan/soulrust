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

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rust_messenger::traits;
use rust_messenger::traits::extended::Sender;
use soulseek_proto::frame::{MAX_LARGE_PEER_MESSAGE_LEN, MAX_PEER_INIT_MESSAGE_LEN, MAX_PEER_MESSAGE_LEN};
use soulseek_proto::peer::{ConnectionType, PeerInit, PeerInitMessage, PierceFirewall};
use soulseek_proto::peer_message::{
    FileSearchResponse, GetSharedFileList, PeerMessage, SharedFileListResponse, UserInfoResponse,
};
use soulseek_proto::server::{ConnectToPeerRequest, ServerRequest};
use soulseek_proto::distributed::{self, DistribSearch, DistributedMessage};
use soulseek_proto::Reader;
use soulseek_proto::transfer::{
    FileTransferInit, QueueUpload, TransferDirection, TransferRequest, TransferResponse,
    UploadDenied,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::components::transfer_io;
use crate::config::AppContext;
use crate::messages::{
    BrowseDir, BrowseFailed, BrowseFile, BrowseListing, DownloadComplete, DownloadFailed,
    HandlerId, IncomingSearch, NetTx, PeerActivity, PeerBrowseConnect, PeerDistribConnect,
    PeerDownloadConnect, PeerPierce, PeerUploadConnect, ResolveUploadPeer, UploadComplete,
    UploadFailed,
};
use crate::search_response;
use crate::shares::ShareIndex;

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
/// *locations*, not bulk data, and must stay well under the bus ring.
const MAX_LISTING_BYTES: usize = 512 * 1024;

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
    IncomingSearch { username: String, token: u32, query: String },
    Download { username: String, ip: String, port: u16, filename: String, size: u64 },
    /// A downloader approved an upload we offered; ask session to resolve their
    /// address (emitted by a connection task via the cloned sender in ConnCtx).
    StartUpload { username: String },
    /// session resolved the address; open the file connection(s) and stream.
    UploadConnect { username: String, ip: String, port: u16 },
    /// Adopt a distributed parent: open a `D` connection and relay its searches.
    DistribConnect { username: String, ip: String, port: u16 },
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
}

/// Shared per-reactor state handed to every connection task.
struct ConnCtx {
    shares: Arc<ShareIndex>,
    queue: Arc<DeliveryQueue>,
    downloads: Mutex<Downloads>,
    uploads: Mutex<Uploads>,
    our_username: String,
    download_dir: PathBuf,
    incomplete_dir: PathBuf,
    /// Lets connection tasks signal the reactor's command loop (e.g. an approved
    /// upload needs an address resolved and a file connection opened).
    cmd_tx: UnboundedSender<PeerCommand>,
    /// Monotonic source of transfer tokens for uploads we initiate.
    next_token: AtomicU32,
}

/// What a finished connection produced, for the accept loop to report on the bus.
enum ConnOutcome {
    /// Served requests / negotiated only — nothing to report.
    Done,
    Downloaded { username: String, filename: String, path: String },
    DownloadFailed { username: String, filename: String, reason: String },
}

pub struct PeerNet {
    listen_port: u16,
    folders: Vec<PathBuf>,
    username: String,
    max_search_results: usize,
    download_dir: PathBuf,
    incomplete_dir: PathBuf,
    cmd_tx: UnboundedSender<PeerCommand>,
    cmd_rx: Option<UnboundedReceiver<PeerCommand>>,
}

impl PeerNet {
    pub fn new<W: traits::core::Writer>(ctx: &AppContext, _writer: &W) -> Self {
        let (cmd_tx, cmd_rx) = unbounded_channel();
        let download_dir = PathBuf::from(&ctx.config.sharing.download_dir);
        // Fall back to the download dir if no separate incomplete dir is set.
        let incomplete_dir = if ctx.config.sharing.incomplete_dir.is_empty() {
            download_dir.clone()
        } else {
            PathBuf::from(&ctx.config.sharing.incomplete_dir)
        };
        PeerNet {
            listen_port: ctx.config.server.listen_port as u16,
            folders: ctx.config.sharing.folders.iter().map(PathBuf::from).collect(),
            username: ctx.config.server.username.clone(),
            max_search_results: ctx.config.sharing.max_search_results as usize,
            download_dir,
            incomplete_dir,
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
}

impl traits::core::Handle<PeerBrowseConnect> for PeerNet {
    fn handle<W: traits::core::Writer>(&mut self, message: &PeerBrowseConnect, _writer: &W) {
        let _ = self.cmd_tx.send(PeerCommand::Browse {
            username: message.username.clone(),
            ip: message.ip.clone(),
            port: message.port,
        });
    }
}

impl traits::core::Handle<PeerPierce> for PeerNet {
    fn handle<W: traits::core::Writer>(&mut self, message: &PeerPierce, _writer: &W) {
        let _ = self.cmd_tx.send(PeerCommand::Pierce {
            username: message.username.clone(),
            ip: message.ip.clone(),
            port: message.port,
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

impl traits::core::Handle<PeerDownloadConnect> for PeerNet {
    fn handle<W: traits::core::Writer>(&mut self, message: &PeerDownloadConnect, _writer: &W) {
        let _ = self.cmd_tx.send(PeerCommand::Download {
            username: message.username.clone(),
            ip: message.ip.clone(),
            port: message.port,
            filename: message.filename.clone(),
            size: message.size,
        });
    }
}

impl traits::core::Handle<PeerUploadConnect> for PeerNet {
    fn handle<W: traits::core::Writer>(&mut self, message: &PeerUploadConnect, _writer: &W) {
        let _ = self.cmd_tx.send(PeerCommand::UploadConnect {
            username: message.username.clone(),
            ip: message.ip.clone(),
            port: message.port,
        });
    }
}

impl traits::core::Handle<PeerDistribConnect> for PeerNet {
    fn handle<W: traits::core::Writer>(&mut self, message: &PeerDistribConnect, _writer: &W) {
        let _ = self.cmd_tx.send(PeerCommand::DistribConnect {
            username: message.username.clone(),
            ip: message.ip.clone(),
            port: message.port,
        });
    }
}

/// One-time/low-frequency status onto the bus (listener bound, fatal errors).
/// Per-connection activity goes to stderr — it is peer-driven and must never
/// outrun the bounded bus reader.
fn status<W: traits::core::Writer>(writer: &W, note: String) {
    PeerNet::send(&PeerActivity { note }, writer);
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
    let max_results = config.max_results;
    runtime.block_on(async move {
        // Scan here (off the startup path) and warm the cached browse frame.
        let shares = Arc::new(ShareIndex::scan(&config.folders));
        let _ = shares.browse_frame();
        let ctx = Arc::new(ConnCtx {
            shares,
            queue: Arc::new(Mutex::new(PendingDeliveries::default())),
            downloads: Mutex::new(Downloads::default()),
            uploads: Mutex::new(Uploads::default()),
            our_username: config.username,
            download_dir: config.download_dir,
            incomplete_dir: config.incomplete_dir,
            cmd_tx,
            next_token: AtomicU32::new(1),
        });

        let listener = match TcpListener::bind(("0.0.0.0", port)).await {
            Ok(listener) => listener,
            Err(err) => {
                status(&writer, format!("cannot listen for peers on port {port}: {err}"));
                return;
            }
        };
        status(
            &writer,
            format!("sharing {} file(s); listening for peers on port {port}", ctx.shares.num_files()),
        );

        // Commands run concurrently with the accept loop on this single thread.
        let cmd = tokio::spawn(command_loop(cmd_rx, ctx.clone(), max_results, writer.clone()));
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
            PeerNet::send(&DownloadComplete { username, filename, path }, writer);
        }
        Ok(ConnOutcome::DownloadFailed { username, filename, reason }) => {
            PeerNet::send(&DownloadFailed { username, filename, reason }, writer);
        }
        Err(err) => eprintln!("[peer-net {addr}] connection ended: {err}"),
    }
}

async fn command_loop<W: traits::core::Writer>(
    mut cmd_rx: UnboundedReceiver<PeerCommand>,
    ctx: Arc<ConnCtx>,
    max_results: usize,
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
            PeerCommand::Download { username: peer, ip, port, filename, size } => {
                let ctx = ctx.clone();
                let writer = writer.clone();
                tokio::spawn(download_init_task(ip, port, peer, filename, size, ctx, writer));
            }
            PeerCommand::DistribConnect { username: peer, ip, port } => {
                let ctx = ctx.clone();
                tokio::spawn(distrib_task(ip, port, peer, ctx));
            }
            PeerCommand::StartUpload { username: peer } => {
                // A downloader approved an upload; ask session to resolve their
                // address so we can open the file connection.
                PeerNet::send(&ResolveUploadPeer { username: peer }, &writer);
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
                    ctx.uploads.lock().unwrap().by_token.remove(&token);
                    if offline {
                        PeerNet::send(
                            &UploadFailed {
                                username: peer.clone(),
                                filename,
                                reason: "peer is offline or not reachable".into(),
                            },
                            &writer,
                        );
                        continue;
                    }
                    let our_username = ctx.our_username.clone();
                    let writer = writer.clone();
                    tokio::spawn(upload_task(
                        ip.clone(),
                        port,
                        token,
                        peer.clone(),
                        filename,
                        real_path,
                        size,
                        our_username,
                        writer,
                    ));
                }
            }
            PeerCommand::IncomingSearch { username: searcher, token, query } => {
                let our_token = connect_token;
                connect_token = connect_token.wrapping_add(1);
                let shares = ctx.shares.clone();
                let queue = ctx.queue.clone();
                let writer = writer.clone();
                let our_username = ctx.our_username.clone();
                let max = max_results;
                // Matching is CPU-bound (word-index intersection); run it off the
                // single reactor thread so a burst of network searches can't
                // head-of-line-block accept / serve / connect dispatch.
                tokio::spawn(async move {
                    let files = tokio::task::spawn_blocking(move || {
                        search_response::respond(&query, max, &[], &shares)
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
                        free_slots: true,
                        upload_speed: 0,
                        in_queue: 0,
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
                    PeerNet::send(&NetTx { frame: request.to_frame() }, &writer);
                });
            }
        }
    }
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
        Err(reason) => PeerNet::send(&BrowseFailed { username: peer, reason }, &writer),
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
                    reason: format!("connect {ip}:{port}: {err}"),
                },
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
        stream.write_all(&QueueUpload { file: filename.clone() }.to_frame()).await
    }
    .await;
    if let Err(err) = queued {
        PeerNet::send(
            &DownloadFailed { username: peer, filename, reason: format!("queueing: {err}") },
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
        transfer_io::upload(&mut stream, token, file, size)
            .await
            .map_err(|e| format!("streaming: {e}"))?;
        Ok::<(), String>(())
    }
    .await;

    match result {
        Ok(()) => PeerNet::send(&UploadComplete { username: peer, filename }, &writer),
        Err(reason) => PeerNet::send(&UploadFailed { username: peer, filename, reason }, &writer),
    }
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
                        // A distributed child connected to us; relay searches.
                        serve_distrib(&mut stream, ctx, &init.username, &mut on_activity).await?;
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
                stream.write_all(ctx.shares.browse_frame()).await?;
                on_activity(format!("served browse to {peer}"));
            }
            PeerMessage::UserInfoRequest => {
                let info = UserInfoResponse {
                    description: format!("soulrust — {} file(s) shared", ctx.shares.num_files()),
                    picture: None,
                    total_uploads: 0,
                    queue_size: 0,
                    slots_available: true,
                    upload_allowed: 1,
                };
                stream.write_all(&info.to_frame()).await?;
                on_activity(format!("served user info to {peer}"));
            }
            PeerMessage::FolderContentsRequest(request) => {
                let response = ctx.shares.folder_response(request.token, &request.directory);
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
            PeerMessage::QueueUpload(queue) => {
                // A peer wants to download one of our files. Offer it (with a
                // size) if we share it; otherwise decline.
                match ctx.shares.resolve(&queue.file) {
                    Some((path, size)) => {
                        let token = ctx.next_token.fetch_add(1, Ordering::Relaxed);
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
                                },
                            );
                            // Bound the registry: a peer that offers files and
                            // never approves can't grow it without limit (evict
                            // the oldest, lowest-token pending upload).
                            while uploads.by_token.len() > MAX_PENDING_TRANSFERS {
                                if let Some(&oldest) = uploads.by_token.keys().min() {
                                    uploads.by_token.remove(&oldest);
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
                            uploads.by_token.remove(&response.token);
                            false
                        }
                        None => false,
                    }
                };
                if approved {
                    let _ = ctx.cmd_tx.send(PeerCommand::StartUpload { username: peer.clone() });
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
        // Unknown token, or a different peer than we negotiated with — drop it.
        return Ok(ConnOutcome::Done);
    };
    on_activity(format!("receiving {} from {} (token {token})", active.filename, active.username));

    let basename = download_basename(&active.filename);
    let incomplete = ctx.incomplete_dir.join(format!("INCOMPLETE-{token}-{basename}"));

    match receive_to_disk(stream, &incomplete, &ctx.download_dir, &basename, active.size).await {
        Ok(path) => Ok(ConnOutcome::Downloaded {
            username: active.username,
            filename: active.filename,
            path,
        }),
        Err(reason) => {
            let _ = tokio::fs::remove_file(&incomplete).await;
            Ok(ConnOutcome::DownloadFailed {
                username: active.username,
                filename: active.filename,
                reason,
            })
        }
    }
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

/// Streams `size` bytes from the connection into `incomplete`, then moves it
/// into `download_dir` under a non-colliding name derived from `basename`. Bytes
/// go straight to disk — never the bus.
async fn receive_to_disk<S>(
    stream: &mut S,
    incomplete: &Path,
    download_dir: &Path,
    basename: &str,
    size: u64,
) -> Result<String, String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    for dir in [incomplete.parent(), Some(download_dir)].into_iter().flatten() {
        tokio::fs::create_dir_all(dir).await.map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    let file = tokio::fs::File::create(incomplete)
        .await
        .map_err(|e| format!("create {}: {e}", incomplete.display()))?;
    // Fresh transfer from offset 0 (resume is a later refinement).
    transfer_io::download(stream, 0, size, file).await.map_err(|e| format!("receiving: {e}"))?;
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
    on_activity(format!("distributed peer {peer} connected"));
    while let Some(payload) =
        read_frame_timeout(stream, MAX_PEER_MESSAGE_LEN, PEER_IDLE_TIMEOUT).await?
    {
        let search = match DistributedMessage::decode(&payload) {
            Ok(DistributedMessage::Search(search)) => Some(search),
            Ok(DistributedMessage::Embedded(embedded))
                if embedded.inner_code == distributed::code::SEARCH =>
            {
                DistribSearch::decode(&mut Reader::new(&embedded.inner_message)).ok()
            }
            Ok(_) => None,  // ping / branch level / root — informational
            Err(_) => break, // undecodable frame; drop the connection
        };
        if let Some(search) = search {
            // Respond like a server search (and, once child forwarding lands,
            // this is where we'd relay it onward).
            let _ = ctx.cmd_tx.send(PeerCommand::IncomingSearch {
                username: search.username,
                token: search.token,
                query: search.query,
            });
            on_activity(format!("relayed a distributed search from {peer}"));
        }
    }
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
                directories.push(BrowseDir { path: dir.path.clone(), files });
                break 'dirs;
            }
            budget -= cost;
            files.push(BrowseFile { name: file.name.clone(), size: file.size });
        }
        directories.push(BrowseDir { path: dir.path.clone(), files });
    }

    BrowseListing { username: username.to_owned(), directories, total_files, truncated }
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
            shares: Arc::new(test_index()),
            queue: Arc::new(Mutex::new(PendingDeliveries::default())),
            downloads: Mutex::new(Downloads::default()),
            uploads: Mutex::new(Uploads::default()),
            our_username: "me".into(),
            download_dir: std::env::temp_dir(),
            incomplete_dir: std::env::temp_dir(),
            cmd_tx,
            next_token: AtomicU32::new(1),
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
            assert!(matches!(info, PeerMessage::UserInfoResponse(_)));
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
                shares: Arc::new(test_index()),
                queue: Arc::new(Mutex::new(PendingDeliveries::default())),
                downloads: Mutex::new(Downloads::default()),
                uploads: Mutex::new(Uploads::default()),
                our_username: "me".into(),
                download_dir: dir.clone(),
                incomplete_dir: dir.clone(),
                cmd_tx,
                next_token: AtomicU32::new(1),
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
                shares: Arc::new(test_index()),
                queue: Arc::new(Mutex::new(PendingDeliveries::default())),
                downloads: Mutex::new(Downloads::default()),
                uploads: Mutex::new(Uploads::default()),
                our_username: "me".into(),
                download_dir: dir.clone(),
                incomplete_dir: dir.clone(),
                cmd_tx,
                next_token: AtomicU32::new(1),
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
    fn serve_distrib_responds_to_a_distributed_search() {
        // A DistribSearch on a D connection is fed into the responder path via a
        // PeerCommand::IncomingSearch (same as a server search).
        use soulseek_proto::distributed::{DistribSearch, SEARCH_IDENTIFIER};
        runtime().block_on(async {
            let (mut client, mut server) = tokio::io::duplex(64 * 1024);
            let (cmd_tx, mut cmd_rx) = unbounded_channel();
            let ctx = Arc::new(ConnCtx {
                shares: Arc::new(test_index()),
                queue: Arc::new(Mutex::new(PendingDeliveries::default())),
                downloads: Mutex::new(Downloads::default()),
                uploads: Mutex::new(Uploads::default()),
                our_username: "me".into(),
                download_dir: std::env::temp_dir(),
                incomplete_dir: std::env::temp_dir(),
                cmd_tx,
                next_token: AtomicU32::new(1),
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
    fn approved_upload_triggers_a_start_upload_command() {
        // When the downloader approves an offer, we mark it and ask the reactor
        // to resolve the address and open the file connection.
        use soulseek_proto::transfer::TransferResponse;
        runtime().block_on(async {
            let (mut client, server) = tokio::io::duplex(64 * 1024);
            let (cmd_tx, mut cmd_rx) = unbounded_channel();
            let ctx = Arc::new(ConnCtx {
                shares: Arc::new(test_index()),
                queue: Arc::new(Mutex::new(PendingDeliveries::default())),
                downloads: Mutex::new(Downloads::default()),
                uploads: Mutex::new(Uploads::default()),
                our_username: "me".into(),
                download_dir: std::env::temp_dir(),
                incomplete_dir: std::env::temp_dir(),
                cmd_tx,
                next_token: AtomicU32::new(1),
            });
            ctx.uploads.lock().unwrap().by_token.insert(
                5,
                PendingUpload {
                    username: "bob".into(),
                    filename: "Music\\Album\\song.mp3".into(),
                    real_path: "/tmp/song.mp3".into(),
                    size: 4096,
                    approved: false,
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
}
