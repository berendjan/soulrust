//! Client ↔ server messages.
//!
//! Message codes and field layouts follow the Nicotine+ protocol reference
//! (<https://nicotine-plus.org/doc/SLSKPROTOCOL.html>). This is a foundation
//! covering the session-establishment and search messages; the remaining
//! codes follow the same pattern.

use std::net::Ipv4Addr;

use crate::frame::frame_u32;
use crate::wire::{put_string, put_u32, DecodeError, Reader};

pub mod code {
    pub const LOGIN: u32 = 1;
    pub const SET_WAIT_PORT: u32 = 2;
    pub const GET_PEER_ADDRESS: u32 = 3;
    pub const FILE_SEARCH: u32 = 26;
}

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

/// A decoded server→client message. Unrecognized codes are surfaced rather
/// than dropped so callers can log or count them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerMessage {
    Login(LoginResponse),
    GetPeerAddress(GetPeerAddressResponse),
    FileSearch(FileSearchBroadcast),
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
            code::FILE_SEARCH => ServerMessage::FileSearch(FileSearchBroadcast::decode(&mut r)?),
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
