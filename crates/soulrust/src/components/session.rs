//! The Soulseek session state machine: pure protocol logic over the bus.
//! Socket I/O lives in [`crate::components::net_edge`]; this component only
//! consumes decoded frame payloads (`NetRx`) and produces outgoing frames
//! (`NetTx`), which keeps it fully unit-testable with a capturing writer.

use std::collections::{HashMap, HashSet};

use rust_messenger::traits;
use rust_messenger::traits::extended::Sender;
use soulseek_proto::distributed::{self, DistribSearch};
use soulseek_proto::peer::ConnectionType;
use soulseek_proto::server::{
    AcceptChildren, BranchLevel, BranchRoot, FileSearchRequest, GetPeerAddressRequest,
    HaveNoParent, LoginRequest, ServerMessage, ServerRequest, SetWaitPort,
};
use soulseek_proto::Reader;

use crate::config::AppContext;
use crate::messages::{
    BrowseAccepted, BrowseFailed, BrowseUser, DownloadFailed, HandlerId, IncomingSearch, NetConn,
    NetConnEvent, NetRx, NetTx, PeerBrowseConnect, PeerDistribConnect, PeerDownloadConnect,
    PeerPierce, PeerUploadConnect, ResolveUploadPeer, SessionEvent, SessionEventKind,
    StartDownload, StartSearch, StartSearchResult, StartedSearch,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionState {
    Disconnected,
    AwaitingLogin,
    LoggedIn,
}

pub struct Session {
    username: String,
    password: String,
    listen_port: u32,
    state: SessionState,
    next_token: u32,
    /// Usernames for which we've asked the server for a peer address and are
    /// waiting to start a browse. A GetPeerAddress response for one of these
    /// triggers a peer connection rather than just a log note.
    pending_browses: HashSet<String>,
    /// Downloads waiting on a peer address, keyed by username. A GetPeerAddress
    /// response drains these into PeerDownloadConnect messages.
    pending_downloads: HashMap<String, Vec<PendingDownload>>,
    /// Usernames whose address we're resolving so peer_net can open an upload
    /// connection. A GetPeerAddress response emits PeerUploadConnect for these.
    pending_upload_resolves: HashSet<String>,
    /// The distributed parent we've asked peer_net to adopt, if any. We adopt at
    /// most one (Nicotine+ does the same) rather than stacking a connection per
    /// PossibleParents message.
    distrib_parent: Option<String>,
}

/// A download queued before we knew the peer's address.
struct PendingDownload {
    filename: String,
    size: u64,
}

/// Client identity sent in the Login message.
const MAJOR_VERSION: u32 = 160;
const MINOR_VERSION: u32 = 1;

impl Session {
    pub fn new<W: traits::core::Writer>(ctx: &AppContext, _writer: &W) -> Self {
        Session {
            username: ctx.config.server.username.clone(),
            password: ctx.config.server.password.clone(),
            listen_port: ctx.config.server.listen_port,
            state: SessionState::Disconnected,
            next_token: 1,
            pending_browses: HashSet::new(),
            pending_downloads: HashMap::new(),
            pending_upload_resolves: HashSet::new(),
            distrib_parent: None,
        }
    }

    fn emit<W: traits::core::Writer>(kind: SessionEventKind, writer: &W) {
        Self::send(&SessionEvent { kind }, writer);
    }
}

impl traits::core::Handler for Session {
    type Id = HandlerId;
    const ID: HandlerId = HandlerId::Session;
}

impl traits::core::Handle<NetConn> for Session {
    fn handle<W: traits::core::Writer>(&mut self, message: &NetConn, writer: &W) {
        match &message.event {
            NetConnEvent::Connected => {
                let login = LoginRequest {
                    username: self.username.clone(),
                    password: self.password.clone(),
                    major_version: MAJOR_VERSION,
                    minor_version: MINOR_VERSION,
                };
                Self::send(&NetTx { frame: login.to_frame() }, writer);
                self.state = SessionState::AwaitingLogin;
                Self::emit(SessionEventKind::Connecting, writer);
            }
            NetConnEvent::Failed { reason } | NetConnEvent::Closed { reason } => {
                self.state = SessionState::Disconnected;
                // Forget the distributed parent so a reconnect re-adopts one.
                self.distrib_parent = None;
                Self::emit(SessionEventKind::Disconnected { reason: reason.clone() }, writer);
            }
        }
    }
}

impl traits::core::Handle<NetRx> for Session {
    fn handle<W: traits::core::Writer>(&mut self, message: &NetRx, writer: &W) {
        let decoded = match ServerMessage::decode(&message.payload) {
            Ok(decoded) => decoded,
            Err(err) => {
                Self::emit(
                    SessionEventKind::ProtocolNote { note: format!("undecodable frame: {err}") },
                    writer,
                );
                return;
            }
        };

        match decoded {
            ServerMessage::Login(soulseek_proto::server::LoginResponse::Success {
                greeting,
                own_ip,
                ..
            }) => {
                self.state = SessionState::LoggedIn;
                let wait_port = SetWaitPort {
                    port: self.listen_port,
                    obfuscation_type: 0,
                    obfuscated_port: 0,
                };
                Self::send(&NetTx { frame: wait_port.to_frame() }, writer);
                // Join the distributed search tree, in Nicotine+'s order: ask for
                // a parent (so the server sends PossibleParents and routes
                // searches to us), declare we're our own root at level 0, and
                // decline children for now (we don't forward down-tree yet).
                Self::send(&NetTx { frame: HaveNoParent { no_parent: true }.to_frame() }, writer);
                Self::send(
                    &NetTx { frame: BranchRoot { root: self.username.clone() }.to_frame() },
                    writer,
                );
                Self::send(&NetTx { frame: BranchLevel { level: 0 }.to_frame() }, writer);
                Self::send(&NetTx { frame: AcceptChildren { accept: false }.to_frame() }, writer);
                Self::emit(
                    SessionEventKind::LoggedIn { greeting, own_ip: own_ip.to_string() },
                    writer,
                );
            }
            ServerMessage::Login(soulseek_proto::server::LoginResponse::Failure {
                reason,
                detail,
            }) => {
                self.state = SessionState::Disconnected;
                let reason = match detail {
                    Some(detail) => format!("{reason}: {detail}"),
                    None => reason,
                };
                Self::emit(SessionEventKind::LoginFailed { reason }, writer);
            }
            ServerMessage::FileSearch(broadcast) => {
                // Hand the search to peer_net to match against our shares and
                // deliver results to the searcher.
                Self::send(
                    &IncomingSearch {
                        username: broadcast.username.clone(),
                        token: broadcast.token,
                        query: broadcast.query.clone(),
                    },
                    writer,
                );
                Self::emit(
                    SessionEventKind::SearchBroadcastSeen {
                        username: broadcast.username,
                        query: broadcast.query,
                    },
                    writer,
                );
            }
            ServerMessage::EmbeddedMessage(embedded) => {
                // We're a branch root: the server injects distributed searches
                // directly. Respond to them like any other search.
                if embedded.distrib_code == distributed::code::SEARCH {
                    if let Ok(search) =
                        DistribSearch::decode(&mut Reader::new(&embedded.distrib_message))
                    {
                        Self::send(
                            &IncomingSearch {
                                username: search.username,
                                token: search.token,
                                query: search.query,
                            },
                            writer,
                        );
                    }
                }
            }
            ServerMessage::PossibleParents(possible) => {
                // Adopt at most one parent. The server resends PossibleParents
                // periodically; without this guard each message would stack up
                // another D connection.
                if self.distrib_parent.is_none() {
                    if let Some(parent) = possible.parents.into_iter().next() {
                        self.distrib_parent = Some(parent.username.clone());
                        Self::send(
                            &PeerDistribConnect {
                                username: parent.username,
                                ip: parent.ip.to_string(),
                                port: parent.port as u16,
                            },
                            writer,
                        );
                    }
                }
            }
            ServerMessage::ParentMinSpeed(_) | ServerMessage::ParentSpeedRatio(_) => {
                // Eligibility/limits — informational for now.
            }
            ServerMessage::GetPeerAddress(response) => {
                // 0.0.0.0:0 is the server's way of saying "unknown/offline".
                let offline = response.ip.is_unspecified() || response.port == 0;
                let ip = response.ip.to_string();
                let port = response.port as u16;

                let was_browse = self.pending_browses.remove(&response.username);
                let downloads =
                    self.pending_downloads.remove(&response.username).unwrap_or_default();
                let was_upload = self.pending_upload_resolves.remove(&response.username);
                let handled = was_browse || !downloads.is_empty() || was_upload;

                if was_browse {
                    if offline {
                        Self::send(
                            &BrowseFailed {
                                username: response.username.clone(),
                                reason: "user is offline or not reachable".into(),
                            },
                            writer,
                        );
                    } else {
                        Self::send(
                            &PeerBrowseConnect {
                                username: response.username.clone(),
                                ip: ip.clone(),
                                port,
                            },
                            writer,
                        );
                    }
                }

                for download in downloads {
                    if offline {
                        Self::send(
                            &DownloadFailed {
                                username: response.username.clone(),
                                filename: download.filename,
                                reason: "user is offline or not reachable".into(),
                            },
                            writer,
                        );
                    } else {
                        Self::send(
                            &PeerDownloadConnect {
                                username: response.username.clone(),
                                ip: ip.clone(),
                                port,
                                filename: download.filename,
                                size: download.size,
                            },
                            writer,
                        );
                    }
                }

                if was_upload {
                    // peer_net checks the offline sentinel (0.0.0.0:0) and fails
                    // the queued uploads itself (it holds their filenames).
                    Self::send(
                        &PeerUploadConnect { username: response.username.clone(), ip: ip.clone(), port },
                        writer,
                    );
                }

                if !handled {
                    Self::emit(
                        SessionEventKind::ProtocolNote {
                            note: format!(
                                "peer address: {} at {}:{}",
                                response.username, response.ip, response.port
                            ),
                        },
                        writer,
                    );
                }
            }
            ServerMessage::ConnectToPeer(request) => {
                // A (likely firewalled) peer wants to reach us. For peer ('P')
                // connections, hand it to peer_net to dial back + pierce. File
                // and distributed connections are handled in later stages.
                if request.connection_type == ConnectionType::Peer {
                    Self::send(
                        &PeerPierce {
                            username: request.username,
                            ip: request.ip.to_string(),
                            port: request.port as u16,
                            token: request.token,
                        },
                        writer,
                    );
                } else {
                    Self::emit(
                        SessionEventKind::ProtocolNote {
                            note: format!(
                                "connect-to-peer ({}) from {} not yet handled",
                                request.connection_type, request.username
                            ),
                        },
                        writer,
                    );
                }
            }
            ServerMessage::ExcludedSearchPhrases(excluded) => {
                Self::emit(
                    SessionEventKind::ProtocolNote {
                        note: format!(
                            "server sent {} excluded search phrase(s)",
                            excluded.phrases.len()
                        ),
                    },
                    writer,
                );
            }
            ServerMessage::Unknown { code, body } => {
                Self::emit(
                    SessionEventKind::ProtocolNote {
                        note: format!("unhandled server message code {code} ({} bytes)", body.len()),
                    },
                    writer,
                );
            }
        }
    }
}

impl traits::core::Handle<StartSearch> for Session {
    fn handle<W: traits::core::Writer>(&mut self, message: &StartSearch, writer: &W) {
        if self.state != SessionState::LoggedIn {
            Self::send(
                &StartSearchResult {
                    corr: message.corr,
                    started: Vec::new(),
                    error: Some("not logged in to the soulseek server".into()),
                },
                writer,
            );
            return;
        }

        let mut started = Vec::new();
        for job in &message.jobs {
            let query = job.to_query();
            if query.is_empty() {
                continue;
            }
            let token = self.next_token;
            self.next_token += 1;
            let request = FileSearchRequest { token, query: query.clone() };
            Self::send(&NetTx { frame: request.to_frame() }, writer);
            Self::emit(
                SessionEventKind::SearchStarted { token, query: query.clone() },
                writer,
            );
            started.push(StartedSearch { token, query });
        }

        Self::send(
            &StartSearchResult { corr: message.corr, started, error: None },
            writer,
        );
    }
}

impl traits::core::Handle<BrowseUser> for Session {
    fn handle<W: traits::core::Writer>(&mut self, message: &BrowseUser, writer: &W) {
        let username = message.username.trim();
        if self.state != SessionState::LoggedIn {
            Self::send(
                &BrowseAccepted {
                    corr: message.corr,
                    error: Some("not logged in to the soulseek server".into()),
                },
                writer,
            );
            return;
        }
        if username.is_empty() {
            Self::send(
                &BrowseAccepted {
                    corr: message.corr,
                    error: Some("enter a username to browse".into()),
                },
                writer,
            );
            return;
        }

        // Ask the server where this peer lives; the GetPeerAddress response
        // (handled above) opens the peer connection.
        let request = GetPeerAddressRequest { username: username.to_owned() };
        Self::send(&NetTx { frame: request.to_frame() }, writer);
        self.pending_browses.insert(username.to_owned());
        Self::send(&BrowseAccepted { corr: message.corr, error: None }, writer);
    }
}

impl traits::core::Handle<StartDownload> for Session {
    fn handle<W: traits::core::Writer>(&mut self, message: &StartDownload, writer: &W) {
        let username = message.username.trim();
        if self.state != SessionState::LoggedIn {
            Self::send(
                &DownloadFailed {
                    username: message.username.clone(),
                    filename: message.filename.clone(),
                    reason: "not logged in to the soulseek server".into(),
                },
                writer,
            );
            return;
        }
        if username.is_empty() || message.filename.is_empty() {
            Self::send(
                &DownloadFailed {
                    username: message.username.clone(),
                    filename: message.filename.clone(),
                    reason: "a username and filename are required".into(),
                },
                writer,
            );
            return;
        }

        // Resolve the peer's address; the GetPeerAddress response drains the
        // pending download into a PeerDownloadConnect.
        let request = GetPeerAddressRequest { username: username.to_owned() };
        Self::send(&NetTx { frame: request.to_frame() }, writer);
        self.pending_downloads.entry(username.to_owned()).or_default().push(PendingDownload {
            filename: message.filename.clone(),
            size: message.size,
        });
    }
}

impl traits::core::Handle<ResolveUploadPeer> for Session {
    fn handle<W: traits::core::Writer>(&mut self, message: &ResolveUploadPeer, writer: &W) {
        if self.state != SessionState::LoggedIn {
            return;
        }
        // Resolve the address; the GetPeerAddress response emits PeerUploadConnect.
        let request = GetPeerAddressRequest { username: message.username.clone() };
        Self::send(&NetTx { frame: request.to_frame() }, writer);
        self.pending_upload_resolves.insert(message.username.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::extract::SearchJob;
    use crate::messages::MessageId;
    use rust_messenger::traits::core::Handle;
    use std::net::Ipv4Addr;
    use std::sync::{Arc, Mutex};

    /// Captures every message written to the bus as (message id, payload).
    #[derive(Clone, Default)]
    struct CapturingWriter {
        records: Arc<Mutex<Vec<(u16, Vec<u8>)>>>,
    }

    impl traits::core::Writer for CapturingWriter {
        fn write<
            M: traits::core::Message,
            H: traits::core::Handler,
            F: FnOnce(&mut [u8]),
        >(
            &self,
            size: usize,
            callback: F,
        ) {
            let mut buf = vec![0u8; size];
            callback(&mut buf);
            self.records.lock().unwrap().push((M::ID.into(), buf));
        }
    }

    impl CapturingWriter {
        fn frames(&self) -> Vec<Vec<u8>> {
            self.records
                .lock()
                .unwrap()
                .iter()
                .filter(|(id, _)| *id == u16::from(MessageId::NetTx))
                .map(|(_, buf)| NetTx::deserialize_from(buf).frame)
                .collect()
        }

        fn events(&self) -> Vec<SessionEventKind> {
            self.records
                .lock()
                .unwrap()
                .iter()
                .filter(|(id, _)| *id == u16::from(MessageId::SessionEvent))
                .map(|(_, buf)| SessionEvent::deserialize_from(buf).kind)
                .collect()
        }

        fn search_results(&self) -> Vec<StartSearchResult> {
            self.records
                .lock()
                .unwrap()
                .iter()
                .filter(|(id, _)| *id == u16::from(MessageId::StartSearchResult))
                .map(|(_, buf)| StartSearchResult::deserialize_from(buf))
                .collect()
        }

        fn browse_accepts(&self) -> Vec<BrowseAccepted> {
            self.decode(MessageId::BrowseAccepted, BrowseAccepted::deserialize_from)
        }

        fn peer_browse_connects(&self) -> Vec<PeerBrowseConnect> {
            self.decode(MessageId::PeerBrowseConnect, PeerBrowseConnect::deserialize_from)
        }

        fn browse_failures(&self) -> Vec<BrowseFailed> {
            self.decode(MessageId::BrowseFailed, BrowseFailed::deserialize_from)
        }

        fn incoming_searches(&self) -> Vec<IncomingSearch> {
            self.decode(MessageId::IncomingSearch, IncomingSearch::deserialize_from)
        }

        fn pierces(&self) -> Vec<PeerPierce> {
            self.decode(MessageId::PeerPierce, PeerPierce::deserialize_from)
        }

        fn download_connects(&self) -> Vec<PeerDownloadConnect> {
            self.decode(MessageId::PeerDownloadConnect, PeerDownloadConnect::deserialize_from)
        }

        fn download_failures(&self) -> Vec<DownloadFailed> {
            self.decode(MessageId::DownloadFailed, DownloadFailed::deserialize_from)
        }

        fn distrib_connects(&self) -> Vec<PeerDistribConnect> {
            self.decode(MessageId::PeerDistribConnect, PeerDistribConnect::deserialize_from)
        }

        fn decode<T>(&self, id: MessageId, f: impl Fn(&[u8]) -> T) -> Vec<T> {
            self.records
                .lock()
                .unwrap()
                .iter()
                .filter(|(rid, _)| *rid == u16::from(id))
                .map(|(_, buf)| f(buf))
                .collect()
        }
    }

    fn get_peer_address_payload(username: &str, ip: Ipv4Addr, port: u32) -> Vec<u8> {
        use soulseek_proto::wire::{put_ipv4, put_string, put_u16, put_u32};
        let mut body = Vec::new();
        put_u32(&mut body, 3); // GetPeerAddress code
        put_string(&mut body, username);
        put_ipv4(&mut body, ip);
        put_u32(&mut body, port);
        put_u32(&mut body, 0); // obfuscation type
        put_u16(&mut body, 0); // obfuscated port
        body
    }

    fn logged_in_session(writer: &CapturingWriter) -> Session {
        let mut session = test_session();
        session.handle(&NetConn { event: NetConnEvent::Connected }, writer);
        session.handle(&NetRx { payload: login_success_payload() }, writer);
        session
    }

    fn test_session() -> Session {
        let mut config = Config::default();
        config.server.username = "testuser".into();
        config.server.password = "testpass".into();
        let ctx = AppContext::new(config, "/tmp/unused.yaml".into());
        Session::new(&ctx, &CapturingWriter::default())
    }

    fn login_success_payload() -> Vec<u8> {
        use soulseek_proto::wire::{put_bool, put_ipv4, put_string, put_u32};
        let mut body = Vec::new();
        put_u32(&mut body, 1); // login code
        put_bool(&mut body, true);
        put_string(&mut body, "Welcome!");
        put_ipv4(&mut body, Ipv4Addr::new(203, 0, 113, 9));
        put_string(&mut body, "0123456789abcdef0123456789abcdef");
        put_bool(&mut body, false);
        body
    }

    #[test]
    fn connect_sends_byte_exact_login_frame() {
        let writer = CapturingWriter::default();
        let mut session = test_session();
        session.handle(&NetConn { event: NetConnEvent::Connected }, &writer);

        let expected = LoginRequest {
            username: "testuser".into(),
            password: "testpass".into(),
            major_version: 160,
            minor_version: 1,
        }
        .to_frame();
        assert_eq!(writer.frames(), vec![expected]);
        assert_eq!(writer.events(), vec![SessionEventKind::Connecting]);
    }

    #[test]
    fn login_success_sets_wait_port_and_emits_logged_in() {
        let writer = CapturingWriter::default();
        let mut session = test_session();
        session.handle(&NetConn { event: NetConnEvent::Connected }, &writer);
        session.handle(&NetRx { payload: login_success_payload() }, &writer);

        let frames = writer.frames();
        // login + wait port + the 4 distributed-tree join messages.
        assert_eq!(frames.len(), 6, "login + wait port + HaveNoParent/BranchRoot/BranchLevel/AcceptChildren");
        let expected_wait_port =
            SetWaitPort { port: 2234, obfuscation_type: 0, obfuscated_port: 0 }.to_frame();
        assert_eq!(frames[1], expected_wait_port);

        assert!(matches!(
            writer.events().last(),
            Some(SessionEventKind::LoggedIn { greeting, own_ip })
                if greeting == "Welcome!" && own_ip == "203.0.113.9"
        ));
    }

    #[test]
    fn login_failure_emits_reason_with_detail() {
        use soulseek_proto::wire::{put_bool, put_string, put_u32};
        let writer = CapturingWriter::default();
        let mut session = test_session();
        session.handle(&NetConn { event: NetConnEvent::Connected }, &writer);

        let mut body = Vec::new();
        put_u32(&mut body, 1);
        put_bool(&mut body, false);
        put_string(&mut body, "INVALIDPASS");
        session.handle(&NetRx { payload: body }, &writer);

        assert!(matches!(
            writer.events().last(),
            Some(SessionEventKind::LoginFailed { reason }) if reason == "INVALIDPASS"
        ));
    }

    #[test]
    fn search_before_login_returns_error_not_frames() {
        let writer = CapturingWriter::default();
        let mut session = test_session();
        session.handle(
            &StartSearch {
                corr: 5,
                source_label: "x".into(),
                jobs: vec![SearchJob { raw_query: Some("q".into()), ..Default::default() }],
            },
            &writer,
        );

        assert!(writer.frames().is_empty());
        let results = writer.search_results();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].corr, 5);
        assert!(results[0].error.as_deref().unwrap().contains("not logged in"));
    }

    #[test]
    fn search_when_logged_in_sends_one_frame_per_job_with_fresh_tokens() {
        let writer = CapturingWriter::default();
        let mut session = test_session();
        session.handle(&NetConn { event: NetConnEvent::Connected }, &writer);
        session.handle(&NetRx { payload: login_success_payload() }, &writer);

        session.handle(
            &StartSearch {
                corr: 9,
                source_label: "spotify".into(),
                jobs: vec![
                    SearchJob {
                        artist: Some("A".into()),
                        title: Some("One".into()),
                        ..Default::default()
                    },
                    SearchJob { raw_query: Some("B Two".into()), ..Default::default() },
                ],
            },
            &writer,
        );

        let frames = writer.frames();
        // login + wait port + 4 distributed-join messages + 2 searches
        assert_eq!(frames.len(), 8);
        assert_eq!(frames[6], FileSearchRequest { token: 1, query: "A One".into() }.to_frame());
        assert_eq!(frames[7], FileSearchRequest { token: 2, query: "B Two".into() }.to_frame());

        let results = writer.search_results();
        assert_eq!(results[0].started.len(), 2);
        assert_eq!(results[0].error, None);
        assert!(writer
            .events()
            .iter()
            .any(|e| matches!(e, SessionEventKind::SearchStarted { token: 1, query } if query == "A One")));
    }

    #[test]
    fn search_broadcast_and_unknown_codes_become_events() {
        use soulseek_proto::wire::{put_string, put_u32};
        let writer = CapturingWriter::default();
        let mut session = test_session();

        let mut body = Vec::new();
        put_u32(&mut body, 26); // file search code
        put_string(&mut body, "bob");
        put_u32(&mut body, 99);
        put_string(&mut body, "some query");
        session.handle(&NetRx { payload: body }, &writer);

        let mut unknown = Vec::new();
        put_u32(&mut unknown, 9999);
        session.handle(&NetRx { payload: unknown }, &writer);

        let events = writer.events();
        assert!(matches!(
            &events[0],
            SessionEventKind::SearchBroadcastSeen { username, query }
                if username == "bob" && query == "some query"
        ));
        assert!(matches!(
            &events[1],
            SessionEventKind::ProtocolNote { note } if note.contains("9999")
        ));
    }

    #[test]
    fn browse_before_login_is_rejected_without_network_traffic() {
        let writer = CapturingWriter::default();
        let mut session = test_session();
        session.handle(&BrowseUser { corr: 7, username: "alice".into() }, &writer);

        assert!(writer.frames().is_empty());
        let accepts = writer.browse_accepts();
        assert_eq!(accepts.len(), 1);
        assert_eq!(accepts[0].corr, 7);
        assert!(accepts[0].error.as_deref().unwrap().contains("not logged in"));
    }

    #[test]
    fn browse_when_logged_in_requests_peer_address_and_accepts() {
        let writer = CapturingWriter::default();
        let mut session = logged_in_session(&writer);
        session.handle(&BrowseUser { corr: 3, username: "  alice  ".into() }, &writer);

        // login + wait port + GetPeerAddress request.
        let frames = writer.frames();
        assert_eq!(
            frames.last().unwrap(),
            &GetPeerAddressRequest { username: "alice".into() }.to_frame(),
            "trimmed username is looked up"
        );
        let accepts = writer.browse_accepts();
        assert_eq!(accepts.last().unwrap().error, None);
    }

    #[test]
    fn peer_address_for_a_pending_browse_triggers_a_peer_connection() {
        let writer = CapturingWriter::default();
        let mut session = logged_in_session(&writer);
        session.handle(&BrowseUser { corr: 1, username: "alice".into() }, &writer);
        session.handle(
            &NetRx { payload: get_peer_address_payload("alice", Ipv4Addr::new(198, 51, 100, 7), 2234) },
            &writer,
        );

        let connects = writer.peer_browse_connects();
        assert_eq!(connects.len(), 1);
        assert_eq!(connects[0].username, "alice");
        assert_eq!(connects[0].ip, "198.51.100.7");
        assert_eq!(connects[0].port, 2234);
        assert!(writer.browse_failures().is_empty());
    }

    #[test]
    fn offline_peer_address_fails_the_browse() {
        let writer = CapturingWriter::default();
        let mut session = logged_in_session(&writer);
        session.handle(&BrowseUser { corr: 1, username: "ghost".into() }, &writer);
        session.handle(
            &NetRx { payload: get_peer_address_payload("ghost", Ipv4Addr::UNSPECIFIED, 0) },
            &writer,
        );

        assert!(writer.peer_browse_connects().is_empty());
        let failures = writer.browse_failures();
        assert_eq!(failures.len(), 1);
        assert!(failures[0].reason.contains("offline"));
    }

    #[test]
    fn unrelated_peer_address_stays_a_log_note() {
        let writer = CapturingWriter::default();
        let mut session = logged_in_session(&writer);
        // No pending browse for this user → it should just be a protocol note.
        session.handle(
            &NetRx { payload: get_peer_address_payload("stranger", Ipv4Addr::new(1, 2, 3, 4), 99) },
            &writer,
        );
        assert!(writer.peer_browse_connects().is_empty());
        assert!(writer
            .events()
            .iter()
            .any(|e| matches!(e, SessionEventKind::ProtocolNote { note } if note.contains("stranger"))));
    }

    #[test]
    fn login_declares_our_branch_position() {
        // On login we join the distributed tree: ask for a parent, declare we're
        // our own root at level 0, and decline children.
        let writer = CapturingWriter::default();
        let _session = logged_in_session(&writer);
        let frames = writer.frames();
        assert!(
            frames.contains(&HaveNoParent { no_parent: true }.to_frame()),
            "HaveNoParent(true) makes the server engage us in the distributed tree"
        );
        assert!(frames.contains(&BranchLevel { level: 0 }.to_frame()), "BranchLevel(0) on login");
        assert!(
            frames.contains(&BranchRoot { root: "testuser".into() }.to_frame()),
            "BranchRoot(self) on login"
        );
        assert!(
            frames.contains(&AcceptChildren { accept: false }.to_frame()),
            "AcceptChildren(false) — we don't forward down-tree yet"
        );
    }

    #[test]
    fn possible_parents_adopts_at_most_one_and_ignores_empty() {
        use soulseek_proto::wire::{put_ipv4, put_string, put_u32};
        let writer = CapturingWriter::default();
        let mut session = logged_in_session(&writer);

        let parents_payload = |entries: &[(&str, Ipv4Addr, u32)]| {
            let mut body = Vec::new();
            put_u32(&mut body, 102);
            put_u32(&mut body, entries.len() as u32);
            for (user, ip, port) in entries {
                put_string(&mut body, user);
                put_ipv4(&mut body, *ip);
                put_u32(&mut body, *port);
            }
            body
        };

        // Empty list -> no connect.
        session.handle(&NetRx { payload: parents_payload(&[]) }, &writer);
        assert!(writer.distrib_connects().is_empty(), "empty PossibleParents adopts nothing");

        // First non-empty list -> one connect.
        session.handle(
            &NetRx { payload: parents_payload(&[("p1", Ipv4Addr::new(10, 0, 0, 1), 1)]) },
            &writer,
        );
        // A second message (server resends) -> still only one parent adopted.
        session.handle(
            &NetRx { payload: parents_payload(&[("p2", Ipv4Addr::new(10, 0, 0, 2), 2)]) },
            &writer,
        );
        let connects = writer.distrib_connects();
        assert_eq!(connects.len(), 1, "we adopt at most one parent");
        assert_eq!(connects[0].username, "p1");
    }

    #[test]
    fn embedded_distributed_search_is_forwarded_for_response() {
        use soulseek_proto::wire::{put_string, put_u32, put_u8};
        let writer = CapturingWriter::default();
        let mut session = logged_in_session(&writer);

        // Server EmbeddedMessage (code 93): u8 inner code 3 (DistribSearch) then
        // its body (identifier=49, username, token, query).
        let mut body = Vec::new();
        put_u32(&mut body, 93); // EmbeddedMessage
        put_u8(&mut body, 3); // inner DistribSearch
        put_u32(&mut body, 49); // identifier (ASCII '1')
        put_string(&mut body, "searcher");
        put_u32(&mut body, 0x4242);
        put_string(&mut body, "distributed query");
        session.handle(&NetRx { payload: body }, &writer);

        let searches = writer.incoming_searches();
        assert_eq!(searches.len(), 1, "the embedded distributed search is responded to");
        assert_eq!(searches[0].username, "searcher");
        assert_eq!(searches[0].token, 0x4242);
        assert_eq!(searches[0].query, "distributed query");
    }

    #[test]
    fn possible_parents_triggers_a_distrib_connect() {
        use soulseek_proto::wire::{put_ipv4, put_string, put_u32};
        let writer = CapturingWriter::default();
        let mut session = logged_in_session(&writer);

        let mut body = Vec::new();
        put_u32(&mut body, 102); // PossibleParents
        put_u32(&mut body, 1); // one parent
        put_string(&mut body, "parent1");
        put_ipv4(&mut body, Ipv4Addr::new(10, 0, 0, 1));
        put_u32(&mut body, 2234);
        session.handle(&NetRx { payload: body }, &writer);

        let connects = writer.distrib_connects();
        assert_eq!(connects.len(), 1);
        assert_eq!(connects[0].username, "parent1");
        assert_eq!(connects[0].ip, "10.0.0.1");
        assert_eq!(connects[0].port, 2234);
    }

    #[test]
    fn file_search_broadcast_is_forwarded_as_incoming_search() {
        use soulseek_proto::wire::{put_string, put_u32};
        let writer = CapturingWriter::default();
        let mut session = test_session();
        let mut body = Vec::new();
        put_u32(&mut body, 26); // FileSearch
        put_string(&mut body, "bob");
        put_u32(&mut body, 99);
        put_string(&mut body, "rust album");
        session.handle(&NetRx { payload: body }, &writer);

        let searches = writer.incoming_searches();
        assert_eq!(searches.len(), 1);
        assert_eq!(searches[0].username, "bob");
        assert_eq!(searches[0].token, 99);
        assert_eq!(searches[0].query, "rust album");
    }

    #[test]
    fn start_download_resolves_address_then_emits_peer_download_connect() {
        let writer = CapturingWriter::default();
        let mut session = logged_in_session(&writer);
        session.handle(
            &StartDownload {
                username: "alice".into(),
                filename: "Music\\song.mp3".into(),
                size: 4096,
            },
            &writer,
        );
        // Looks the peer up.
        assert_eq!(
            writer.frames().last().unwrap(),
            &GetPeerAddressRequest { username: "alice".into() }.to_frame()
        );

        // The address response drains the pending download into a connect.
        session.handle(
            &NetRx { payload: get_peer_address_payload("alice", Ipv4Addr::new(198, 51, 100, 7), 2234) },
            &writer,
        );
        let connects = writer.download_connects();
        assert_eq!(connects.len(), 1);
        assert_eq!(connects[0].username, "alice");
        assert_eq!(connects[0].ip, "198.51.100.7");
        assert_eq!(connects[0].port, 2234);
        assert_eq!(connects[0].filename, "Music\\song.mp3");
        assert_eq!(connects[0].size, 4096);
    }

    #[test]
    fn start_download_from_an_offline_user_fails() {
        let writer = CapturingWriter::default();
        let mut session = logged_in_session(&writer);
        session.handle(
            &StartDownload { username: "ghost".into(), filename: "a.mp3".into(), size: 1 },
            &writer,
        );
        session.handle(
            &NetRx { payload: get_peer_address_payload("ghost", Ipv4Addr::UNSPECIFIED, 0) },
            &writer,
        );
        let failures = writer.download_failures();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].filename, "a.mp3");
        assert!(writer.download_connects().is_empty());
    }

    #[test]
    fn start_download_when_not_logged_in_fails_immediately() {
        let writer = CapturingWriter::default();
        let mut session = test_session(); // not logged in
        session.handle(
            &StartDownload { username: "alice".into(), filename: "a.mp3".into(), size: 1 },
            &writer,
        );
        let failures = writer.download_failures();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].reason, "not logged in to the soulseek server");
    }

    #[test]
    fn connect_to_peer_for_a_peer_connection_triggers_a_pierce() {
        use soulseek_proto::wire::{put_bool, put_ipv4, put_string, put_u32};
        let writer = CapturingWriter::default();
        let mut session = test_session();
        let mut body = Vec::new();
        put_u32(&mut body, 18); // ConnectToPeer
        put_string(&mut body, "carol");
        put_string(&mut body, "P");
        put_ipv4(&mut body, Ipv4Addr::new(10, 0, 0, 5));
        put_u32(&mut body, 2234);
        put_u32(&mut body, 555); // token
        put_bool(&mut body, false); // privileged
        put_u32(&mut body, 0); // obfuscation type
        put_u32(&mut body, 0); // obfuscated port
        session.handle(&NetRx { payload: body }, &writer);

        let pierces = writer.pierces();
        assert_eq!(pierces.len(), 1);
        assert_eq!(pierces[0].username, "carol");
        assert_eq!(pierces[0].ip, "10.0.0.5");
        assert_eq!(pierces[0].port, 2234);
        assert_eq!(pierces[0].token, 555);
    }

    #[test]
    fn disconnect_resets_state_so_searches_fail_again() {
        let writer = CapturingWriter::default();
        let mut session = test_session();
        session.handle(&NetConn { event: NetConnEvent::Connected }, &writer);
        session.handle(&NetRx { payload: login_success_payload() }, &writer);
        session.handle(
            &NetConn { event: NetConnEvent::Closed { reason: "eof".into() } },
            &writer,
        );

        session.handle(
            &StartSearch {
                corr: 1,
                source_label: "x".into(),
                jobs: vec![SearchJob { raw_query: Some("q".into()), ..Default::default() }],
            },
            &writer,
        );
        assert!(writer.search_results()[0].error.is_some());
        assert!(writer
            .events()
            .iter()
            .any(|e| matches!(e, SessionEventKind::Disconnected { reason } if reason == "eof")));
    }
}
