//! Client ↔ server messages.
//!
//! Message codes and field layouts follow the Nicotine+ protocol reference
//! (<https://nicotine-plus.org/doc/SLSKPROTOCOL.html>). This is a foundation
//! covering the session-establishment and search messages; the remaining
//! codes follow the same pattern.

use std::net::Ipv4Addr;

use crate::frame::frame_u32;
use crate::peer::ConnectionType;
use crate::wire::{put_string, put_u32, DecodeError, Reader};

pub mod code {
    pub const LOGIN: u32 = 1;
    pub const SET_WAIT_PORT: u32 = 2;
    pub const GET_PEER_ADDRESS: u32 = 3;
    pub const CONNECT_TO_PEER: u32 = 18;
    pub const FILE_SEARCH: u32 = 26;
    pub const EXCLUDED_SEARCH_PHRASES: u32 = 160;
}

/// Cap on a server-supplied list length, so a forged count can't make us
/// preallocate an arbitrary vector.
const MAX_LIST_PREALLOC: usize = 4096;

/// A message the client sends to the server.
pub trait ServerRequest {
    const CODE: u32;

    fn encode_body(&self, buf: &mut Vec<u8>);

    /// The complete wire frame: `[length][code][body]`.
    fn to_frame(&self) -> Vec<u8> {
        let mut body = Vec::new();
        self.encode_body(&mut body);
        frame_u32(Self::CODE, &body)
    }
}

/// Server code 1 — Login request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
    /// Client major version (e.g. 160 for Nicotine+).
    pub major_version: u32,
    pub minor_version: u32,
}

impl ServerRequest for LoginRequest {
    const CODE: u32 = code::LOGIN;

    fn encode_body(&self, buf: &mut Vec<u8>) {
        put_string(buf, &self.username);
        put_string(buf, &self.password);
        put_u32(buf, self.major_version);
        // MD5 hex digest of username + password, sent as a string.
        let digest = md5::compute(format!("{}{}", self.username, self.password));
        put_string(buf, &format!("{digest:x}"));
        put_u32(buf, self.minor_version);
    }
}

/// Server code 1 — Login response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginResponse {
    Success {
        /// Server message of the day.
        greeting: String,
        /// The client's own IP address as seen by the server.
        own_ip: Ipv4Addr,
        /// MD5 hex digest of the password.
        password_md5: String,
        is_supporter: bool,
    },
    Failure {
        /// Rejection code, e.g. `INVALIDUSERNAME` or `INVALIDPASS`.
        reason: String,
        /// Present only for some reasons (e.g. `INVALIDUSERNAME`).
        detail: Option<String>,
    },
}

impl LoginResponse {
    pub fn decode(r: &mut Reader) -> Result<Self, DecodeError> {
        if r.bool()? {
            Ok(LoginResponse::Success {
                greeting: r.string()?,
                own_ip: r.ipv4()?,
                password_md5: r.string()?,
                is_supporter: r.bool()?,
            })
        } else {
            let reason = r.string()?;
            let detail = if r.is_empty() { None } else { Some(r.string()?) };
            Ok(LoginResponse::Failure { reason, detail })
        }
    }
}

/// Server code 2 — SetWaitPort: the port this client listens on for peer
/// connections. No response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetWaitPort {
    pub port: u32,
    /// 0 = no obfuscation supported, 1 = rotated obfuscation.
    pub obfuscation_type: u32,
    pub obfuscated_port: u32,
}

impl ServerRequest for SetWaitPort {
    const CODE: u32 = code::SET_WAIT_PORT;

    fn encode_body(&self, buf: &mut Vec<u8>) {
        // SoulseekQt form: the port followed by the obfuscation fields. We don't
        // implement obfuscated connections, so obfuscation_type and
        // obfuscated_port are 0, but we send them as SoulseekQt does. Nicotine+
        // instead sends the port alone; the server accepts both (verified by the
        // soulfind_protocol integration test).
        put_u32(buf, self.port);
        put_u32(buf, self.obfuscation_type);
        put_u32(buf, self.obfuscated_port);
    }
}

/// Server code 3 — GetPeerAddress request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetPeerAddressRequest {
    pub username: String,
}

impl ServerRequest for GetPeerAddressRequest {
    const CODE: u32 = code::GET_PEER_ADDRESS;

    fn encode_body(&self, buf: &mut Vec<u8>) {
        put_string(buf, &self.username);
    }
}

/// Server code 3 — GetPeerAddress response. An ip/port of 0.0.0.0:0 means
/// the user is offline or unknown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetPeerAddressResponse {
    pub username: String,
    pub ip: Ipv4Addr,
    pub port: u32,
    pub obfuscation_type: u32,
    /// Note: u16 on the wire, unlike the u32 in SetWaitPort.
    pub obfuscated_port: u16,
}

impl GetPeerAddressResponse {
    pub fn decode(r: &mut Reader) -> Result<Self, DecodeError> {
        Ok(GetPeerAddressResponse {
            username: r.string()?,
            ip: r.ipv4()?,
            port: r.u32()?,
            obfuscation_type: r.u32()?,
            obfuscated_port: r.u16()?,
        })
    }
}

/// Server code 26 — FileSearch request (client initiates a network search).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSearchRequest {
    /// Client-generated identifier echoed in peer search replies.
    pub token: u32,
    pub query: String,
}

impl ServerRequest for FileSearchRequest {
    const CODE: u32 = code::FILE_SEARCH;

    fn encode_body(&self, buf: &mut Vec<u8>) {
        put_u32(buf, self.token);
        put_string(buf, &self.query);
    }
}

/// Server code 26 — FileSearch as received from the server: a search by
/// another user being relayed to this client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSearchBroadcast {
    pub username: String,
    pub token: u32,
    pub query: String,
}

impl FileSearchBroadcast {
    pub fn decode(r: &mut Reader) -> Result<Self, DecodeError> {
        Ok(FileSearchBroadcast {
            username: r.string()?,
            token: r.u32()?,
            query: r.string()?,
        })
    }
}

/// Server code 18 — ConnectToPeer request: ask the server to relay an indirect
/// connection request to `username` (used when we can't reach a peer directly).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectToPeerRequest {
    pub token: u32,
    pub username: String,
    pub connection_type: ConnectionType,
}

impl ServerRequest for ConnectToPeerRequest {
    const CODE: u32 = code::CONNECT_TO_PEER;

    fn encode_body(&self, buf: &mut Vec<u8>) {
        put_u32(buf, self.token);
        put_string(buf, &self.username);
        put_string(buf, self.connection_type.as_str());
    }
}

/// Server code 18 — ConnectToPeer as received: another peer wants to connect to
/// us and the server is relaying their request. We respond by connecting to
/// `ip:port` and sending a PierceFirewall carrying `token`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectToPeer {
    pub username: String,
    pub connection_type: ConnectionType,
    pub ip: Ipv4Addr,
    pub port: u32,
    pub token: u32,
    pub privileged: bool,
    pub obfuscation_type: u32,
    pub obfuscated_port: u32,
}

impl ConnectToPeer {
    pub fn decode(r: &mut Reader) -> Result<Self, DecodeError> {
        Ok(ConnectToPeer {
            username: r.string()?,
            connection_type: ConnectionType::from_str(&r.string()?)?,
            ip: r.ipv4()?,
            port: r.u32()?,
            token: r.u32()?,
            privileged: r.bool()?,
            obfuscation_type: r.u32()?,
            obfuscated_port: r.u32()?,
        })
    }
}

/// Server code 160 — ExcludedSearchPhrases: phrases the server forbids in search
/// results; we drop any shared file whose path contains one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExcludedSearchPhrases {
    pub phrases: Vec<String>,
}

impl ExcludedSearchPhrases {
    pub fn decode(r: &mut Reader) -> Result<Self, DecodeError> {
        let count = r.u32()? as usize;
        let mut phrases = Vec::with_capacity(count.min(MAX_LIST_PREALLOC));
        for _ in 0..count {
            phrases.push(r.string()?);
        }
        Ok(ExcludedSearchPhrases { phrases })
    }
}

/// A decoded server→client message. Unrecognized codes are surfaced rather
/// than dropped so callers can log or count them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerMessage {
    Login(LoginResponse),
    GetPeerAddress(GetPeerAddressResponse),
    ConnectToPeer(ConnectToPeer),
    FileSearch(FileSearchBroadcast),
    ExcludedSearchPhrases(ExcludedSearchPhrases),
    Unknown { code: u32, body: Vec<u8> },
}

impl ServerMessage {
    /// Decodes a frame payload (message code + contents) as produced by
    /// [`crate::frame::split_frame`].
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(payload);
        let code = r.u32()?;
        Ok(match code {
            code::LOGIN => ServerMessage::Login(LoginResponse::decode(&mut r)?),
            code::GET_PEER_ADDRESS => {
                ServerMessage::GetPeerAddress(GetPeerAddressResponse::decode(&mut r)?)
            }
            code::CONNECT_TO_PEER => ServerMessage::ConnectToPeer(ConnectToPeer::decode(&mut r)?),
            code::FILE_SEARCH => ServerMessage::FileSearch(FileSearchBroadcast::decode(&mut r)?),
            code::EXCLUDED_SEARCH_PHRASES => {
                ServerMessage::ExcludedSearchPhrases(ExcludedSearchPhrases::decode(&mut r)?)
            }
            _ => {
                let mut body = Vec::with_capacity(r.remaining());
                while !r.is_empty() {
                    body.push(r.u8()?);
                }
                ServerMessage::Unknown { code, body }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::split_frame;
    use crate::wire::{put_bool, put_ipv4, put_u16};

    #[test]
    fn login_request_frame_is_byte_exact() {
        let frame = LoginRequest {
            username: "testuser".into(),
            password: "testpass".into(),
            major_version: 160,
            minor_version: 1,
        }
        .to_frame();

        let mut expected = Vec::new();
        put_u32(&mut expected, 1); // code
        put_string(&mut expected, "testuser");
        put_string(&mut expected, "testpass");
        put_u32(&mut expected, 160);
        // md5 hex of "testusertestpass", computed independently with md5sum.
        put_string(&mut expected, "e6f4c2570d30ef3abe2cc30f2b80f01d");
        put_u32(&mut expected, 1);

        let mut framed = Vec::new();
        put_u32(&mut framed, expected.len() as u32);
        framed.extend_from_slice(&expected);
        assert_eq!(frame, framed);
    }

    #[test]
    fn set_wait_port_frame_is_byte_exact() {
        // SoulseekQt form: port + obfuscation_type + obfuscated_port (three
        // u32s). Body is 12 bytes, frame length = code(4) + body(12) = 16.
        let frame = SetWaitPort { port: 2234, obfuscation_type: 0, obfuscated_port: 0 }.to_frame();
        assert_eq!(
            frame,
            [
                0x10, 0x00, 0x00, 0x00, // length 16
                0x02, 0x00, 0x00, 0x00, // code 2
                0xBA, 0x08, 0x00, 0x00, // port 2234
                0x00, 0x00, 0x00, 0x00, // obfuscation type
                0x00, 0x00, 0x00, 0x00, // obfuscated port
            ]
        );
    }

    #[test]
    fn file_search_request_body_is_byte_exact() {
        // Parallels Nicotine+'s FileSearchTest: the encoded body (token +
        // length-prefixed query, no frame header) must be byte-for-byte stable.
        let mut body = Vec::new();
        FileSearchRequest { token: 524700074, query: "70 gwen auto".into() }.encode_body(&mut body);
        assert_eq!(body, b"\xaaIF\x1f\x0c\x00\x00\x0070 gwen auto");
    }

    #[test]
    fn get_peer_address_request_body_is_byte_exact() {
        // Parallels Nicotine+'s GetPeerAddressMessageTest.
        let mut body = Vec::new();
        GetPeerAddressRequest { username: "user1".into() }.encode_body(&mut body);
        assert_eq!(body, b"\x05\x00\x00\x00user1");
    }

    #[test]
    fn connect_to_peer_request_body_is_byte_exact() {
        let mut body = Vec::new();
        ConnectToPeerRequest {
            token: 0x0102_0304,
            username: "alice".into(),
            connection_type: ConnectionType::Peer,
        }
        .encode_body(&mut body);

        let mut expected = Vec::new();
        put_u32(&mut expected, 0x0102_0304);
        put_string(&mut expected, "alice");
        put_string(&mut expected, "P");
        assert_eq!(body, expected);
    }

    #[test]
    fn connect_to_peer_response_decodes() {
        let mut body = Vec::new();
        put_u32(&mut body, code::CONNECT_TO_PEER);
        put_string(&mut body, "bob");
        put_string(&mut body, "F");
        put_ipv4(&mut body, Ipv4Addr::new(1, 2, 3, 4));
        put_u32(&mut body, 2234);
        put_u32(&mut body, 99);
        put_bool(&mut body, true);
        put_u32(&mut body, 0);
        put_u32(&mut body, 0);

        assert_eq!(
            ServerMessage::decode(&body).unwrap(),
            ServerMessage::ConnectToPeer(ConnectToPeer {
                username: "bob".into(),
                connection_type: ConnectionType::File,
                ip: Ipv4Addr::new(1, 2, 3, 4),
                port: 2234,
                token: 99,
                privileged: true,
                obfuscation_type: 0,
                obfuscated_port: 0,
            })
        );
    }

    #[test]
    fn excluded_search_phrases_decodes() {
        let mut body = Vec::new();
        put_u32(&mut body, code::EXCLUDED_SEARCH_PHRASES);
        put_u32(&mut body, 2);
        put_string(&mut body, "linux distro");
        put_string(&mut body, "netbsd");

        assert_eq!(
            ServerMessage::decode(&body).unwrap(),
            ServerMessage::ExcludedSearchPhrases(ExcludedSearchPhrases {
                phrases: vec!["linux distro".into(), "netbsd".into()],
            })
        );
    }

    #[test]
    fn login_success_response_decodes() {
        let mut body = Vec::new();
        put_u32(&mut body, code::LOGIN);
        put_bool(&mut body, true);
        put_string(&mut body, "Welcome to Soulseek!");
        put_ipv4(&mut body, Ipv4Addr::new(203, 0, 113, 7));
        put_string(&mut body, "0123456789abcdef0123456789abcdef");
        put_bool(&mut body, false);

        let msg = ServerMessage::decode(&body).unwrap();
        assert_eq!(
            msg,
            ServerMessage::Login(LoginResponse::Success {
                greeting: "Welcome to Soulseek!".into(),
                own_ip: Ipv4Addr::new(203, 0, 113, 7),
                password_md5: "0123456789abcdef0123456789abcdef".into(),
                is_supporter: false,
            })
        );
    }

    #[test]
    fn login_failure_response_decodes_with_and_without_detail() {
        let mut body = Vec::new();
        put_u32(&mut body, code::LOGIN);
        put_bool(&mut body, false);
        put_string(&mut body, "INVALIDPASS");
        assert_eq!(
            ServerMessage::decode(&body).unwrap(),
            ServerMessage::Login(LoginResponse::Failure {
                reason: "INVALIDPASS".into(),
                detail: None,
            })
        );

        let mut body = Vec::new();
        put_u32(&mut body, code::LOGIN);
        put_bool(&mut body, false);
        put_string(&mut body, "INVALIDUSERNAME");
        put_string(&mut body, "Nick too long.");
        assert_eq!(
            ServerMessage::decode(&body).unwrap(),
            ServerMessage::Login(LoginResponse::Failure {
                reason: "INVALIDUSERNAME".into(),
                detail: Some("Nick too long.".into()),
            })
        );
    }

    #[test]
    fn login_failure_empty_detail_decodes_as_some_empty_string() {
        // Nicotine+'s Login.parse_network_message reads rejection_detail iff
        // `has_remaining_content()` is true — i.e. it keys off whether any bytes
        // remain, NOT off the string's content. A zero-length detail string
        // (its 4-byte length prefix of 0 is still "remaining content") is read
        // as "" rather than skipped, so it must decode to Some("") not None.
        // This is the boundary between an absent field and a present-but-empty
        // one; the existing failure test pins None (no bytes) and Some(non-empty).
        let mut body = Vec::new();
        put_u32(&mut body, code::LOGIN);
        put_bool(&mut body, false);
        put_string(&mut body, "INVALIDUSERNAME");
        put_string(&mut body, ""); // zero-length detail: 4 bytes of length prefix
        assert_eq!(
            ServerMessage::decode(&body).unwrap(),
            ServerMessage::Login(LoginResponse::Failure {
                reason: "INVALIDUSERNAME".into(),
                detail: Some(String::new()),
            })
        );
    }

    #[test]
    fn get_peer_address_response_round_trips_through_frame() {
        let mut body = Vec::new();
        put_u32(&mut body, code::GET_PEER_ADDRESS);
        put_string(&mut body, "alice");
        put_ipv4(&mut body, Ipv4Addr::new(198, 51, 100, 23));
        put_u32(&mut body, 2234);
        put_u32(&mut body, 1);
        put_u16(&mut body, 2235);

        // Wrap in a frame and split it back off, as a receive loop would.
        let mut framed = Vec::new();
        put_u32(&mut framed, body.len() as u32);
        framed.extend_from_slice(&body);
        let (payload, rest) = split_frame(&framed).unwrap().unwrap();
        assert!(rest.is_empty());

        assert_eq!(
            ServerMessage::decode(payload).unwrap(),
            ServerMessage::GetPeerAddress(GetPeerAddressResponse {
                username: "alice".into(),
                ip: Ipv4Addr::new(198, 51, 100, 23),
                port: 2234,
                obfuscation_type: 1,
                obfuscated_port: 2235,
            })
        );
    }

    #[test]
    fn get_peer_address_response_decodes_offline_user_as_zeroes() {
        // Nicotine+'s GetPeerAddress.parse_network_message does no special-casing:
        // it unpacks user/ip/port/obfuscation_type/obfuscated_port straight off
        // the wire (slskmessages.py). The server signals an offline/unknown user
        // with 0.0.0.0 and port 0, which must decode to the zero values verbatim
        // (unpack_ip of four zero bytes is "0.0.0.0", unpack_uint16 of 0 is 0).
        let mut body = Vec::new();
        put_u32(&mut body, code::GET_PEER_ADDRESS);
        put_string(&mut body, "ghost");
        put_ipv4(&mut body, Ipv4Addr::new(0, 0, 0, 0));
        put_u32(&mut body, 0); // port
        put_u32(&mut body, 0); // obfuscation type
        put_u16(&mut body, 0); // obfuscated port
        assert_eq!(
            ServerMessage::decode(&body).unwrap(),
            ServerMessage::GetPeerAddress(GetPeerAddressResponse {
                username: "ghost".into(),
                ip: Ipv4Addr::new(0, 0, 0, 0),
                port: 0,
                obfuscation_type: 0,
                obfuscated_port: 0,
            })
        );
    }

    #[test]
    fn login_success_response_decodes_supporter_true() {
        // The is_supporter trailing field is a bool: Nicotine+ reads it with
        // unpack_bool, so any nonzero byte is true. The existing success test
        // pins the false branch; pin the true branch here so a flipped/dropped
        // trailing bool is caught.
        let mut body = Vec::new();
        put_u32(&mut body, code::LOGIN);
        put_bool(&mut body, true);
        put_string(&mut body, "Welcome to Soulseek!");
        put_ipv4(&mut body, Ipv4Addr::new(203, 0, 113, 7));
        put_string(&mut body, "0123456789abcdef0123456789abcdef");
        put_bool(&mut body, true);

        assert_eq!(
            ServerMessage::decode(&body).unwrap(),
            ServerMessage::Login(LoginResponse::Success {
                greeting: "Welcome to Soulseek!".into(),
                own_ip: Ipv4Addr::new(203, 0, 113, 7),
                password_md5: "0123456789abcdef0123456789abcdef".into(),
                is_supporter: true,
            })
        );
    }

    #[test]
    fn file_search_broadcast_decodes() {
        let mut body = Vec::new();
        put_u32(&mut body, code::FILE_SEARCH);
        put_string(&mut body, "bob");
        put_u32(&mut body, 0xDEAD_BEEF);
        put_string(&mut body, "artist - title");

        assert_eq!(
            ServerMessage::decode(&body).unwrap(),
            ServerMessage::FileSearch(FileSearchBroadcast {
                username: "bob".into(),
                token: 0xDEAD_BEEF,
                query: "artist - title".into(),
            })
        );
    }

    #[test]
    fn unknown_codes_are_preserved() {
        let mut body = Vec::new();
        put_u32(&mut body, 9999);
        body.extend_from_slice(&[1, 2, 3]);
        assert_eq!(
            ServerMessage::decode(&body).unwrap(),
            ServerMessage::Unknown { code: 9999, body: vec![1, 2, 3] }
        );
    }

    #[test]
    fn unknown_code_with_empty_body_is_preserved() {
        // A server message with a code we don't implement but no payload (the
        // cursor sits exactly at the end after reading the code) must surface as
        // Unknown with an empty body — the `while !r.is_empty()` collector simply
        // gathers nothing. Nicotine+ likewise treats an unrecognized code as a
        // pass-through rather than an error; the zero-length boundary must not be
        // mistaken for a truncation (UnexpectedEof). The existing unknown-code
        // test pins a non-empty body; this pins the empty boundary.
        let mut body = Vec::new();
        put_u32(&mut body, 12345);
        assert_eq!(
            ServerMessage::decode(&body).unwrap(),
            ServerMessage::Unknown { code: 12345, body: Vec::new() }
        );
    }

    #[test]
    fn truncated_message_is_an_error_not_a_panic() {
        let mut body = Vec::new();
        put_u32(&mut body, code::GET_PEER_ADDRESS);
        put_u32(&mut body, 100); // string claims 100 bytes, none follow
        assert!(matches!(
            ServerMessage::decode(&body),
            Err(DecodeError::UnexpectedEof { .. })
        ));
    }
}
