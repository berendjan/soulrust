//! The peer-connection edge: opens outbound peer connections and runs the
//! browse exchange, feeding the resulting share listing onto the bus.
//!
//! Mirrors [`crate::components::net_edge`]: socket I/O happens on a spawned
//! thread, and only decoded results (paths + sizes — never file bytes) cross
//! the bus. The protocol exchange is factored into [`browse_over_stream`] so it
//! is testable over any `Read + Write` without a real socket.
//!
//! v1 makes *direct* connections only. Firewalled peers, reached indirectly via
//! the server's ConnectToPeer flow, are a follow-up; a failed connect surfaces
//! as a clear `BrowseFailed` rather than a hang.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use rust_messenger::traits;
use rust_messenger::traits::extended::Sender;
use soulseek_proto::frame::{split_frame_capped, MAX_LARGE_PEER_MESSAGE_LEN};
use soulseek_proto::peer::{ConnectionType, PeerInit};
use soulseek_proto::peer_message::{GetSharedFileList, PeerMessage, SharedFileListResponse};

use crate::config::AppContext;
use crate::messages::{
    BrowseDir, BrowseFailed, BrowseFile, BrowseListing, HandlerId, PeerBrowseConnect,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Byte budget for the listing we forward onto the bus for a single browse.
/// The bus is a fixed-size ring (a few MB), and it carries *locations*, not
/// bulk data — so we bound the listing to comfortably less than the ring rather
/// than risk a single message that doesn't fit. A user sharing a huge library
/// gets a `truncated` listing; the true file count is always reported.
const MAX_LISTING_BYTES: usize = 512 * 1024;

/// Rough encoded-size estimate for one forwarded file: name bytes plus a fixed
/// allowance for the length prefix and the u64 size.
fn file_cost(name: &str) -> usize {
    name.len() + 16
}

pub struct PeerEdge {
    /// Our own Soulseek username, sent in the PeerInit handshake.
    username: String,
}

impl PeerEdge {
    pub fn new<W: traits::core::Writer>(ctx: &AppContext, _writer: &W) -> Self {
        PeerEdge { username: ctx.config.server.username.clone() }
    }
}

impl traits::core::Handler for PeerEdge {
    type Id = HandlerId;
    const ID: HandlerId = HandlerId::PeerEdge;
}

impl traits::core::Handle<PeerBrowseConnect> for PeerEdge {
    fn handle<W: traits::core::Writer>(&mut self, message: &PeerBrowseConnect, writer: &W) {
        let our_username = self.username.clone();
        let peer = message.username.clone();
        let target = format!("{}:{}", message.ip, message.port);
        let writer = writer.clone();

        std::thread::Builder::new()
            .name("soulrust-peer-browse".into())
            .spawn(move || match fetch_shares(&target, &our_username) {
                Ok(response) => {
                    PeerEdge::send(&to_listing(&peer, &response), &writer);
                }
                Err(reason) => {
                    PeerEdge::send(&BrowseFailed { username: peer, reason }, &writer);
                }
            })
            .expect("spawning peer browse thread");
    }
}

/// Connects to `target`, then runs the browse exchange.
fn fetch_shares(target: &str, our_username: &str) -> Result<SharedFileListResponse, String> {
    let addr: SocketAddr = target
        .parse()
        .map_err(|_| format!("peer address {target} is not a usable ip:port (firewalled peers need indirect connections, not yet supported)"))?;
    let mut stream =
        TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(|e| format!("connect {target}: {e}"))?;
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .map_err(|e| format!("set read timeout: {e}"))?;
    browse_over_stream(&mut stream, our_username)
}

/// The browse exchange over an established connection: send the peer-init
/// handshake and the share-list request, then read frames until the shared
/// file list arrives. Other peer messages are ignored; EOF or an oversized
/// accumulation is an error rather than a hang.
fn browse_over_stream<S: Read + Write>(
    stream: &mut S,
    our_username: &str,
) -> Result<SharedFileListResponse, String> {
    let init = PeerInit {
        username: our_username.to_owned(),
        connection_type: ConnectionType::Peer,
        token: 0,
    };
    stream.write_all(&init.to_frame()).map_err(|e| format!("send peer init: {e}"))?;
    stream
        .write_all(&GetSharedFileList.to_frame())
        .map_err(|e| format!("send share-list request: {e}"))?;

    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        let n = stream.read(&mut chunk).map_err(|e| format!("read from peer: {e}"))?;
        if n == 0 {
            return Err("peer closed the connection before sending its share list".into());
        }
        pending.extend_from_slice(&chunk[..n]);
        loop {
            // A SharedFileListResponse is a "large" peer message; split_frame_capped
            // rejects a declared length beyond that cap before we buffer it.
            match split_frame_capped(&pending, MAX_LARGE_PEER_MESSAGE_LEN) {
                Ok(Some((payload, rest))) => {
                    let consumed = pending.len() - rest.len();
                    match PeerMessage::decode(payload) {
                        Ok(PeerMessage::SharedFileList(response)) => return Ok(response),
                        Ok(PeerMessage::Unknown { .. }) => {}
                        Err(err) => return Err(format!("decoding peer message: {err}")),
                    }
                    pending.drain(..consumed);
                }
                Ok(None) => break,
                Err(err) => return Err(format!("framing peer stream: {err}")),
            }
        }
    }
}

/// Maps the decoded protocol response to the bus message, capping the number of
/// files forwarded while still reporting the true total.
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
    use soulseek_proto::frame::split_frame;
    use soulseek_proto::peer::PeerInitMessage;
    use soulseek_proto::peer_message::{SharedDirectory, SharedFile};
    use std::io::Cursor;

    /// A fake peer: captures everything we write, and replays canned bytes on
    /// read (returning EOF once exhausted).
    struct MockPeer {
        written: Vec<u8>,
        to_read: Cursor<Vec<u8>>,
    }

    impl MockPeer {
        fn new(response: Vec<u8>) -> Self {
            MockPeer { written: Vec::new(), to_read: Cursor::new(response) }
        }
    }

    impl Write for MockPeer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Read for MockPeer {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.to_read.read(buf)
        }
    }

    fn sample_response() -> SharedFileListResponse {
        SharedFileListResponse {
            directories: vec![SharedDirectory {
                path: "Music".into(),
                files: vec![
                    SharedFile { name: "a.mp3".into(), size: 100, extension: "mp3".into(), attributes: vec![] },
                    SharedFile { name: "b.mp3".into(), size: 200, extension: "mp3".into(), attributes: vec![] },
                ],
            }],
            private_directories: vec![],
        }
    }

    #[test]
    fn browse_sends_handshake_then_request_and_returns_the_listing() {
        let mut peer = MockPeer::new(sample_response().to_frame());
        let response = browse_over_stream(&mut peer, "me").unwrap();
        assert_eq!(response, sample_response());

        // The first frame we sent must be a PeerInit for a "P" connection as us.
        let (payload, rest) = split_frame(&peer.written).unwrap().unwrap();
        assert_eq!(
            PeerInitMessage::decode(payload).unwrap(),
            PeerInitMessage::PeerInit(PeerInit {
                username: "me".into(),
                connection_type: ConnectionType::Peer,
                token: 0,
            })
        );
        // Followed by the GetSharedFileList request.
        assert_eq!(rest, GetSharedFileList.to_frame());
    }

    #[test]
    fn unknown_peer_messages_before_the_list_are_skipped() {
        let mut bytes = soulseek_proto::frame::frame_u32(99, &[1, 2, 3]);
        bytes.extend_from_slice(&sample_response().to_frame());
        let mut peer = MockPeer::new(bytes);
        assert_eq!(browse_over_stream(&mut peer, "me").unwrap(), sample_response());
    }

    #[test]
    fn eof_before_list_is_an_error() {
        let mut peer = MockPeer::new(Vec::new());
        assert!(browse_over_stream(&mut peer, "me").unwrap_err().contains("closed"));
    }

    #[test]
    fn to_listing_reports_total_and_caps_forwarded_files() {
        let listing = to_listing("alice", &sample_response());
        assert_eq!(listing.total_files, 2);
        assert!(!listing.truncated);
        assert_eq!(listing.directories[0].files.len(), 2);
        assert_eq!(listing.directories[0].files[0].name, "a.mp3");
    }

    #[test]
    fn component_fetches_from_a_real_stub_peer_over_tcp() {
        use crate::messages::MessageId;
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant};

        #[derive(Clone, Default)]
        struct CapturingWriter {
            records: Arc<Mutex<Vec<(u16, Vec<u8>)>>>,
        }
        impl traits::core::Writer for CapturingWriter {
            fn write<M: traits::core::Message, H: traits::core::Handler, F: FnOnce(&mut [u8])>(
                &self,
                size: usize,
                callback: F,
            ) {
                let mut buf = vec![0u8; size];
                callback(&mut buf);
                self.records.lock().unwrap().push((M::ID.into(), buf));
            }
        }

        // A stub peer: accept one connection, verify our peer-init handshake,
        // then serve a real SharedFileListResponse frame.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let served = SharedFileListResponse {
            directories: vec![SharedDirectory {
                path: "Music".into(),
                files: vec![SharedFile {
                    name: "Music\\hit.mp3".into(),
                    size: 4096,
                    extension: String::new(),
                    attributes: vec![],
                }],
            }],
            private_directories: vec![],
        };
        let served_clone = served.clone();
        let stub = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();

            // Read the first frame and confirm it's our PeerInit ("P", as us).
            let mut acc = Vec::new();
            let mut tmp = [0u8; 1024];
            let payload = loop {
                if let Some((p, _)) = split_frame(&acc).unwrap() {
                    break p.to_vec();
                }
                let n = sock.read(&mut tmp).unwrap();
                assert!(n > 0, "client closed before the handshake");
                acc.extend_from_slice(&tmp[..n]);
            };
            assert_eq!(
                PeerInitMessage::decode(&payload).unwrap(),
                PeerInitMessage::PeerInit(PeerInit {
                    username: "tester".into(),
                    connection_type: ConnectionType::Peer,
                    token: 0,
                })
            );

            sock.write_all(&served_clone.to_frame()).unwrap();
            // Drain whatever the client sends next so it doesn't see a reset
            // before it finishes reading our response.
            let _ = sock.read(&mut tmp);
        });

        let writer = CapturingWriter::default();
        let mut edge = PeerEdge { username: "tester".into() };
        traits::core::Handle::<PeerBrowseConnect>::handle(
            &mut edge,
            &PeerBrowseConnect {
                username: "peer".into(),
                ip: addr.ip().to_string(),
                port: addr.port(),
            },
            &writer,
        );

        // The handler fetches on a background thread; wait for the BrowseListing.
        let deadline = Instant::now() + Duration::from_secs(10);
        let listing = loop {
            let found = writer
                .records
                .lock()
                .unwrap()
                .iter()
                .find(|(id, _)| *id == u16::from(MessageId::BrowseListing))
                .map(|(_, buf)| BrowseListing::deserialize_from(buf));
            if let Some(listing) = found {
                break listing;
            }
            assert!(Instant::now() < deadline, "no BrowseListing was produced");
            std::thread::sleep(Duration::from_millis(20));
        };
        stub.join().unwrap();

        assert_eq!(listing.username, "peer");
        assert_eq!(listing.total_files, 1);
        assert!(!listing.truncated);
        assert_eq!(listing.directories[0].path, "Music");
        assert_eq!(listing.directories[0].files[0].name, "Music\\hit.mp3");
        assert_eq!(listing.directories[0].files[0].size, 4096);
    }

    #[test]
    fn to_listing_truncates_a_share_larger_than_the_bus_budget() {
        // One directory with enough files to blow past MAX_LISTING_BYTES.
        let per_file = file_cost("track-000000.flac");
        let count = (MAX_LISTING_BYTES / per_file) + 1000;
        let files: Vec<SharedFile> = (0..count)
            .map(|i| SharedFile {
                name: format!("track-{i:06}.flac"),
                size: 1,
                extension: "flac".into(),
                attributes: vec![],
            })
            .collect();
        let response = SharedFileListResponse {
            directories: vec![SharedDirectory { path: "Huge".into(), files }],
            private_directories: vec![],
        };

        let listing = to_listing("alice", &response);
        assert_eq!(listing.total_files, count as u64, "true count is always reported");
        assert!(listing.truncated, "an oversized share is marked truncated");

        let forwarded: usize = listing.directories.iter().map(|d| d.files.len()).sum();
        assert!(forwarded < count, "fewer files forwarded than exist");
        let bytes: usize =
            listing.directories.iter().flat_map(|d| &d.files).map(|f| file_cost(&f.name)).sum();
        assert!(bytes <= MAX_LISTING_BYTES, "forwarded listing stays within the bus budget");
    }

    #[test]
    fn to_listing_counts_private_directories_in_the_total() {
        let mut response = sample_response();
        response.private_directories = vec![SharedDirectory {
            path: "Buddies".into(),
            files: vec![SharedFile {
                name: "c.mp3".into(),
                size: 1,
                extension: "mp3".into(),
                attributes: vec![],
            }],
        }];
        let listing = to_listing("alice", &response);
        assert_eq!(listing.total_files, 3);
        assert_eq!(listing.directories.len(), 2);
    }
}
