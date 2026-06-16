//! The async peer-network edge: a tokio reactor on a dedicated thread that
//! listens for incoming peer connections and serves them from our share index,
//! bridging to the synchronous bus via the cloneable `Writer` (it only emits
//! lightweight activity events — the bulk share/browse data is built here and
//! written straight to the socket, never onto the bus).
//!
//! This stage handles the *serving* (accept) side: a peer connects to us, sends
//! its peer-init, then asks to browse / for our user info / for a folder's
//! contents, and we answer on the same connection. Search-response delivery
//! (connecting *out* to a searcher) and file transfers come in later stages.
//!
//! The per-connection logic lives in [`serve_connection`], generic over any
//! `AsyncRead + AsyncWrite`, so it is unit-testable over an in-memory duplex
//! without a real socket.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rust_messenger::traits;
use rust_messenger::traits::extended::Sender;
use soulseek_proto::frame::{MAX_PEER_INIT_MESSAGE_LEN, MAX_PEER_MESSAGE_LEN};
use soulseek_proto::peer::PeerInitMessage;
use soulseek_proto::peer_message::{PeerMessage, UserInfoResponse};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::config::AppContext;
use crate::messages::{HandlerId, PeerActivity};
use crate::shares::ShareIndex;

/// Drop a peer connection that sends nothing for this long, so idle/slow peers
/// can't pin connection + task resources indefinitely.
const PEER_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

pub struct PeerNet {
    listen_port: u16,
    /// Folders to share; scanned on the reactor thread (not here) so a large or
    /// slow filesystem walk can't block messenger startup.
    folders: Vec<PathBuf>,
}

impl PeerNet {
    pub fn new<W: traits::core::Writer>(ctx: &AppContext, _writer: &W) -> Self {
        PeerNet {
            listen_port: ctx.config.server.listen_port as u16,
            folders: ctx.config.sharing.folders.iter().map(PathBuf::from).collect(),
        }
    }
}

impl traits::core::Handler for PeerNet {
    type Id = HandlerId;
    const ID: HandlerId = HandlerId::PeerNet;

    fn on_start<W: traits::core::Writer>(&mut self, writer: &W) {
        let port = self.listen_port;
        let folders = std::mem::take(&mut self.folders);
        let writer = writer.clone();
        std::thread::Builder::new()
            .name("soulrust-peer-net".into())
            .spawn(move || run_reactor(port, folders, writer))
            .expect("spawning peer-net reactor thread");
    }
}

/// One-time/low-frequency status onto the bus (listener bound, fatal errors).
/// Per-connection and per-request activity goes to stderr instead — it is
/// peer-driven and unbounded, and must never be able to outrun the bounded bus
/// reader and trigger a "fell behind" panic in the core worker.
fn status<W: traits::core::Writer>(writer: &W, note: String) {
    PeerNet::send(&PeerActivity { note }, writer);
}

fn run_reactor<W: traits::core::Writer>(port: u16, folders: Vec<PathBuf>, writer: W) {
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(err) => {
            status(&writer, format!("peer reactor failed to start: {err}"));
            return;
        }
    };
    runtime.block_on(reactor_loop(port, folders, writer));
}

async fn reactor_loop<W: traits::core::Writer>(port: u16, folders: Vec<PathBuf>, writer: W) {
    // Scan here, on the reactor thread, off the startup path. Warm the cached
    // browse frame so the first browse request doesn't pay the build+compress.
    let shares = Arc::new(ShareIndex::scan(&folders));
    let _ = shares.browse_frame();

    let listener = match TcpListener::bind(("0.0.0.0", port)).await {
        Ok(listener) => listener,
        Err(err) => {
            // Non-fatal: the rest of the app still runs (we just won't be
            // reachable by peers). Common in dev when the port is taken.
            status(&writer, format!("cannot listen for peers on port {port}: {err}"));
            return;
        }
    };
    status(&writer, format!("sharing {} file(s); listening for peers on port {port}", shares.num_files()));

    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                let shares = shares.clone();
                tokio::spawn(async move {
                    if let Err(err) =
                        serve_connection(stream, &shares, |note| eprintln!("[peer-net {addr}] {note}"))
                            .await
                    {
                        eprintln!("[peer-net {addr}] connection ended: {err}");
                    }
                });
            }
            Err(err) => {
                // Transient errors (EMFILE, ECONNABORTED) must not kill the
                // listener for the process lifetime — log, back off, retry.
                eprintln!("[peer-net] accept error: {err}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// Reads one length-prefixed frame, returning the payload (message code +
/// contents, no length prefix), or `None` on a clean end of stream. Rejects a
/// declared length beyond `max_len` for this connection type.
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

/// [`read_frame`] with an idle timeout: returns `Ok(None)` if the peer sends
/// nothing for `idle`, so a silent/slow connection is dropped rather than
/// leaking its task and socket.
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

/// Serves one accepted peer connection: read the peer-init handshake, then
/// answer browse / user-info / folder-contents requests from `shares` until the
/// peer disconnects. `on_activity` reports notable events (a bus emit in
/// production, a collector in tests).
async fn serve_connection<S, F>(
    mut stream: S,
    shares: &ShareIndex,
    mut on_activity: F,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: FnMut(String),
{
    let Some(init_payload) =
        read_frame_timeout(&mut stream, MAX_PEER_INIT_MESSAGE_LEN, PEER_IDLE_TIMEOUT).await?
    else {
        return Ok(());
    };
    let peer = match PeerInitMessage::decode(&init_payload) {
        Ok(PeerInitMessage::PeerInit(init)) => init.username,
        Ok(PeerInitMessage::PierceFirewall(_)) => "<indirect>".to_owned(),
        Err(_) => return Ok(()), // not a peer-init we understand
    };
    on_activity(format!("peer {peer} connected"));

    // Incoming requests are small; cap them at the medium peer limit rather than
    // the 448 MiB large-response cap (which is for browse/search *responses*).
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
            // Responses (e.g. a FileSearchResponse meant for us as a searcher)
            // and not-yet-handled messages are ignored on a serving connection.
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use soulseek_proto::peer::{ConnectionType, PeerInit};
    use soulseek_proto::peer_message::{FolderContentsRequest, GetSharedFileList, UserInfoRequest};
    use std::sync::Mutex;

    fn test_index() -> ShareIndex {
        let mut index = ShareIndex::default();
        index.add_virtual("Music\\Album\\song.mp3", 4096);
        index.add_virtual("Music\\Album\\other.flac", 8192);
        index
    }

    async fn read_one_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Vec<u8> {
        read_frame(reader, MAX_PEER_MESSAGE_LEN).await.unwrap().unwrap()
    }

    /// Runs `serve_connection` on one end of an in-memory duplex while a "peer"
    /// drives the other, and returns the activity log.
    fn drive(requests: Vec<Vec<u8>>, check: impl FnOnce(&mut Vec<Vec<u8>>)) -> Vec<String> {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async move {
            let (mut client, server) = tokio::io::duplex(64 * 1024);
            let acts = Arc::new(Mutex::new(Vec::new()));
            let acts_for_task = acts.clone();
            let shares = Arc::new(test_index());

            let serve = tokio::spawn(async move {
                serve_connection(server, &shares, |note| acts_for_task.lock().unwrap().push(note))
                    .await
            });

            // Peer sends its init then each request.
            let init = PeerInit { username: "peer".into(), connection_type: ConnectionType::Peer, token: 0 };
            client.write_all(&init.to_frame()).await.unwrap();
            for request in &requests {
                client.write_all(request).await.unwrap();
            }
            // Read one response frame per request.
            let mut responses = Vec::new();
            for _ in &requests {
                responses.push(read_one_frame(&mut client).await);
            }
            check(&mut responses);

            drop(client); // EOF → serve loop ends cleanly
            serve.await.unwrap().unwrap();
            Arc::try_unwrap(acts).unwrap().into_inner().unwrap()
        })
    }

    #[test]
    fn serves_browse_to_a_peer() {
        let acts = drive(vec![GetSharedFileList.to_frame()], |responses| {
            let PeerMessage::SharedFileList(list) = PeerMessage::decode(&responses[0]).unwrap()
            else {
                panic!("expected a shared file list");
            };
            let dir = list.directories.iter().find(|d| d.path == "Music\\Album").unwrap();
            let names: Vec<&str> = dir.files.iter().map(|f| f.name.as_str()).collect();
            assert!(names.contains(&"song.mp3")); // folder stream → basename
            assert!(names.contains(&"other.flac"));
        });
        assert!(acts.iter().any(|a| a.contains("peer connected")));
        assert!(acts.iter().any(|a| a.contains("served browse")));
    }

    #[test]
    fn serves_user_info_with_share_count() {
        drive(vec![UserInfoRequest.to_frame()], |responses| {
            let PeerMessage::UserInfoResponse(info) = PeerMessage::decode(&responses[0]).unwrap()
            else {
                panic!("expected user info");
            };
            assert!(info.description.contains("2 file(s)"));
            assert!(info.slots_available);
        });
    }

    #[test]
    fn serves_folder_contents_for_the_requested_folder() {
        let request = FolderContentsRequest { token: 7, directory: "Music\\Album".into() }.to_frame();
        drive(vec![request], |responses| {
            let PeerMessage::FolderContents(resp) = PeerMessage::decode(&responses[0]).unwrap()
            else {
                panic!("expected folder contents");
            };
            assert_eq!(resp.token, 7);
            assert_eq!(resp.directory, "Music\\Album");
            assert_eq!(resp.folders.len(), 1);
            assert_eq!(resp.folders[0].files.len(), 2);
        });
    }

    #[test]
    fn ignores_an_unknown_request_without_responding() {
        // A bare PeerInit with no follow-up requests: just connects and ends.
        let acts = drive(vec![], |responses| assert!(responses.is_empty()));
        assert!(acts.iter().any(|a| a.contains("peer connected")));
    }
}
