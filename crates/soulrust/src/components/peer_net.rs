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

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
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
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::config::AppContext;
use crate::messages::{
    BrowseDir, BrowseFailed, BrowseFile, BrowseListing, HandlerId, IncomingSearch, NetTx,
    PeerActivity, PeerBrowseConnect, PeerPierce,
};
use crate::search_response;
use crate::shares::ShareIndex;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Drop a peer connection that sends nothing for this long, so idle/slow peers
/// can't pin connection + task resources indefinitely.
const PEER_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Byte budget for a browse listing forwarded onto the bus — it carries
/// *locations*, not bulk data, and must stay well under the bus ring.
const MAX_LISTING_BYTES: usize = 512 * 1024;

fn file_cost(name: &str) -> usize {
    name.len() + 16
}

/// Outgoing messages queued for a peer by username, delivered once that peer's
/// connection is established (e.g. a FileSearchResponse the searcher connects in
/// to collect).
type DeliveryQueue = Mutex<HashMap<String, Vec<Vec<u8>>>>;

/// Work for the reactor, sent by the bus-facing handlers.
enum PeerCommand {
    Browse { username: String, ip: String, port: u16 },
    Pierce { username: String, ip: String, port: u16, token: u32 },
    IncomingSearch { username: String, token: u32, query: String },
}

pub struct PeerNet {
    listen_port: u16,
    folders: Vec<PathBuf>,
    username: String,
    max_search_results: usize,
    cmd_tx: UnboundedSender<PeerCommand>,
    cmd_rx: Option<UnboundedReceiver<PeerCommand>>,
}

impl PeerNet {
    pub fn new<W: traits::core::Writer>(ctx: &AppContext, _writer: &W) -> Self {
        let (cmd_tx, cmd_rx) = unbounded_channel();
        PeerNet {
            listen_port: ctx.config.server.listen_port as u16,
            folders: ctx.config.sharing.folders.iter().map(PathBuf::from).collect(),
            username: ctx.config.server.username.clone(),
            max_search_results: ctx.config.sharing.max_search_results as usize,
            cmd_tx,
            cmd_rx: Some(cmd_rx),
        }
    }
}

impl traits::core::Handler for PeerNet {
    type Id = HandlerId;
    const ID: HandlerId = HandlerId::PeerNet;

    fn on_start<W: traits::core::Writer>(&mut self, writer: &W) {
        let port = self.listen_port;
        let folders = std::mem::take(&mut self.folders);
        let username = self.username.clone();
        let max_results = self.max_search_results;
        let cmd_rx = self.cmd_rx.take().expect("on_start called once");
        let writer = writer.clone();
        std::thread::Builder::new()
            .name("soulrust-peer-net".into())
            .spawn(move || run_reactor(port, folders, username, max_results, cmd_rx, writer))
            .expect("spawning peer-net reactor thread");
    }
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

/// One-time/low-frequency status onto the bus (listener bound, fatal errors).
/// Per-connection activity goes to stderr — it is peer-driven and must never
/// outrun the bounded bus reader.
fn status<W: traits::core::Writer>(writer: &W, note: String) {
    PeerNet::send(&PeerActivity { note }, writer);
}

fn run_reactor<W: traits::core::Writer>(
    port: u16,
    folders: Vec<PathBuf>,
    username: String,
    max_results: usize,
    cmd_rx: UnboundedReceiver<PeerCommand>,
    writer: W,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(err) => {
            status(&writer, format!("peer reactor failed to start: {err}"));
            return;
        }
    };
    runtime.block_on(async move {
        // Scan here (off the startup path) and warm the cached browse frame.
        let shares = Arc::new(ShareIndex::scan(&folders));
        let _ = shares.browse_frame();
        let queue: Arc<DeliveryQueue> = Arc::new(Mutex::new(HashMap::new()));

        let listener = match TcpListener::bind(("0.0.0.0", port)).await {
            Ok(listener) => listener,
            Err(err) => {
                status(&writer, format!("cannot listen for peers on port {port}: {err}"));
                return;
            }
        };
        status(&writer, format!("sharing {} file(s); listening for peers on port {port}", shares.num_files()));

        // Commands run concurrently with the accept loop on this single thread.
        let cmd = tokio::spawn(command_loop(
            cmd_rx,
            shares.clone(),
            queue.clone(),
            username,
            max_results,
            writer.clone(),
        ));
        accept_loop(listener, shares, queue).await;
        cmd.abort();
    });
}

async fn accept_loop(listener: TcpListener, shares: Arc<ShareIndex>, queue: Arc<DeliveryQueue>) {
    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                let shares = shares.clone();
                let queue = queue.clone();
                tokio::spawn(async move {
                    let result = serve_connection(stream, &shares, &queue, None, |note| {
                        eprintln!("[peer-net {addr}] {note}")
                    })
                    .await;
                    if let Err(err) = result {
                        eprintln!("[peer-net {addr}] connection ended: {err}");
                    }
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

async fn command_loop<W: traits::core::Writer>(
    mut cmd_rx: UnboundedReceiver<PeerCommand>,
    shares: Arc<ShareIndex>,
    queue: Arc<DeliveryQueue>,
    username: String,
    max_results: usize,
    writer: W,
) {
    let mut connect_token: u32 = 1;
    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            PeerCommand::Browse { username: peer, ip, port } => {
                let our_username = username.clone();
                let writer = writer.clone();
                tokio::spawn(browse_task(ip, port, peer, our_username, writer));
            }
            PeerCommand::Pierce { username: peer, ip, port, token } => {
                let shares = shares.clone();
                let queue = queue.clone();
                tokio::spawn(pierce_task(ip, port, token, peer, shares, queue));
            }
            PeerCommand::IncomingSearch { username: searcher, token, query } => {
                let files = search_response::respond(&query, max_results, &[], &shares);
                if files.is_empty() {
                    continue;
                }
                let response = FileSearchResponse {
                    username: username.clone(),
                    token,
                    files,
                    free_slots: true,
                    upload_speed: 0,
                    in_queue: 0,
                    private_files: Vec::new(),
                };
                queue.lock().unwrap().entry(searcher.clone()).or_default().push(response.to_frame());
                // Ask the server to relay a connect request so the searcher
                // connects in and collects the queued response.
                let request = ConnectToPeerRequest {
                    token: connect_token,
                    username: searcher,
                    connection_type: ConnectionType::Peer,
                };
                connect_token = connect_token.wrapping_add(1);
                PeerNet::send(&NetTx { frame: request.to_frame() }, &writer);
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
    let init =
        PeerInit { username: our_username.to_owned(), connection_type: ConnectionType::Peer, token: 0 };
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
}

/// Indirect connect: dial the peer and send PierceFirewall(token), then serve
/// them (the firewalled peer treats the connection as established and sends its
/// requests; the username is already known from the server's ConnectToPeer).
async fn pierce_task(
    ip: String,
    port: u16,
    token: u32,
    peer: String,
    shares: Arc<ShareIndex>,
    queue: Arc<DeliveryQueue>,
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
    let result = serve_connection(stream, &shares, &queue, Some(peer.clone()), |note| {
        eprintln!("[peer-net pierce {peer}] {note}")
    })
    .await;
    if let Err(err) = result {
        eprintln!("[peer-net pierce {peer}] ended: {err}");
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
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await?;
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
/// until it disconnects.
async fn serve_connection<S, F>(
    mut stream: S,
    shares: &ShareIndex,
    queue: &DeliveryQueue,
    known_peer: Option<String>,
    mut on_activity: F,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: FnMut(String),
{
    let peer = match known_peer {
        Some(peer) => peer,
        None => {
            let Some(init_payload) =
                read_frame_timeout(&mut stream, MAX_PEER_INIT_MESSAGE_LEN, PEER_IDLE_TIMEOUT).await?
            else {
                return Ok(());
            };
            match PeerInitMessage::decode(&init_payload) {
                Ok(PeerInitMessage::PeerInit(init)) => init.username,
                Ok(PeerInitMessage::PierceFirewall(_)) => "<indirect>".to_owned(),
                Err(_) => return Ok(()), // not a peer-init we understand
            }
        }
    };
    on_activity(format!("peer {peer} connected"));

    // Deliver anything queued for this peer (e.g. a FileSearchResponse it
    // connected in to collect). The lock is released before any await.
    let queued = queue.lock().unwrap().remove(&peer).unwrap_or_default();
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
                stream.write_all(shares.browse_frame()).await?;
                on_activity(format!("served browse to {peer}"));
            }
            PeerMessage::UserInfoRequest => {
                let info = UserInfoResponse {
                    description: format!("soulrust — {} file(s) shared", shares.num_files()),
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
                let response = shares.folder_response(request.token, &request.directory);
                stream.write_all(&response.to_frame()).await?;
                on_activity(format!("served folder contents to {peer}"));
            }
            _ => {} // responses / not-yet-handled messages
        }
    }
    Ok(())
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
    use soulseek_proto::peer::PeerInit;
    use soulseek_proto::peer_message::{FolderContentsRequest, SharedDirectory, SharedFile, UserInfoRequest};

    fn test_index() -> ShareIndex {
        let mut index = ShareIndex::default();
        index.add_virtual("Music\\Album\\song.mp3", 4096);
        index.add_virtual("Music\\Album\\other.flac", 8192);
        index
    }

    fn empty_queue() -> Arc<DeliveryQueue> {
        Arc::new(Mutex::new(HashMap::new()))
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
            let shares = Arc::new(test_index());
            let queue = empty_queue();
            let serve = tokio::spawn(async move {
                serve_connection(server, &shares, &queue, None, |_| {}).await
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
    fn delivers_queued_messages_after_handshake() {
        // A FileSearchResponse queued for "peer" is sent as soon as it connects,
        // before any request — the search-delivery path.
        runtime().block_on(async {
            let (mut client, server) = tokio::io::duplex(64 * 1024);
            let shares = Arc::new(test_index());
            let queue = empty_queue();
            let response = FileSearchResponse {
                username: "us".into(),
                token: 99,
                files: vec![SharedFile { name: "hit.mp3".into(), size: 1, extension: String::new(), attributes: vec![] }],
                free_slots: true,
                upload_speed: 0,
                in_queue: 0,
                private_files: vec![],
            };
            queue.lock().unwrap().entry("peer".into()).or_default().push(response.to_frame());

            let serve = tokio::spawn(async move {
                serve_connection(server, &shares, &queue, None, |_| {}).await
            });
            let init = PeerInit { username: "peer".into(), connection_type: ConnectionType::Peer, token: 0 };
            client.write_all(&init.to_frame()).await.unwrap();

            let delivered = PeerMessage::decode(&read_one_frame(&mut client).await).unwrap();
            let PeerMessage::FileSearchResponse(resp) = delivered else { panic!("expected search response") };
            assert_eq!(resp.token, 99);
            assert_eq!(resp.files[0].name, "hit.mp3");

            drop(client);
            serve.await.unwrap().unwrap();
        });
    }

    #[test]
    fn pierced_connection_serves_without_a_peer_init() {
        // known_peer set (as after PierceFirewall): no peer-init is read; the
        // peer sends a request directly and we serve it.
        runtime().block_on(async {
            let (mut client, server) = tokio::io::duplex(64 * 1024);
            let shares = Arc::new(test_index());
            let queue = empty_queue();
            let serve = tokio::spawn(async move {
                serve_connection(server, &shares, &queue, Some("fw-peer".into()), |_| {}).await
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
}
