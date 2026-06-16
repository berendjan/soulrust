//! Peer-init messages: the first message exchanged on a freshly opened
//! peer-to-peer connection. These use a `u8` message code, unlike server
//! and peer messages.

use std::fmt;

use crate::frame::frame_u8;
use crate::wire::{put_string, put_u32, DecodeError, Reader};

pub mod code {
    pub const PIERCE_FIREWALL: u8 = 0;
    pub const PEER_INIT: u8 = 1;
}

/// The kind of peer connection being established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionType {
    /// `P` — peer-to-peer (search results, user info, browse).
    Peer,
    /// `F` — file transfer.
    File,
    /// `D` — distributed search network.
    Distributed,
}

impl ConnectionType {
    pub fn as_str(self) -> &'static str {
        match self {
            ConnectionType::Peer => "P",
            ConnectionType::File => "F",
            ConnectionType::Distributed => "D",
        }
    }

    pub fn from_str(s: &str) -> Result<Self, DecodeError> {
        match s {
            "P" => Ok(ConnectionType::Peer),
            "F" => Ok(ConnectionType::File),
            "D" => Ok(ConnectionType::Distributed),
            other => Err(DecodeError::InvalidValue(format!(
                "unknown connection type {other:?} (expected P, F or D)"
            ))),
        }
    }
}

impl fmt::Display for ConnectionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Peer-init code 0 — PierceFirewall: sent over an indirect connection,
/// carrying the token from the server's ConnectToPeer message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PierceFirewall {
    pub token: u32,
}

impl PierceFirewall {
    pub fn to_frame(&self) -> Vec<u8> {
        let mut body = Vec::new();
        put_u32(&mut body, self.token);
        frame_u8(code::PIERCE_FIREWALL, &body)
    }

    pub fn decode(r: &mut Reader) -> Result<Self, DecodeError> {
        Ok(PierceFirewall { token: r.u32()? })
    }
}

/// Peer-init code 1 — PeerInit: sent when connecting to a peer directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInit {
    /// Username of the connecting client (us, when sending).
    pub username: String,
    pub connection_type: ConnectionType,
    /// Legacy field, always 0 in the modern protocol.
    pub token: u32,
}

impl PeerInit {
    pub fn to_frame(&self) -> Vec<u8> {
        let mut body = Vec::new();
        put_string(&mut body, &self.username);
        put_string(&mut body, self.connection_type.as_str());
        // Nicotine+ PeerInit.make_network_message writes `pack_uint32(0)` — a
        // literal constant ("Empty token"), never the instance's token
        // (slskmessages.py). The legacy token is always zero on the wire today,
        // so we emit 0 unconditionally rather than re-serialising `self.token`
        // (which would diverge from Nicotine+ when re-encoding a decoded legacy
        // frame whose token was non-zero).
        put_u32(&mut body, 0);
        frame_u8(code::PEER_INIT, &body)
    }

    pub fn decode(r: &mut Reader) -> Result<Self, DecodeError> {
        let username = r.string()?;
        let connection_type = ConnectionType::from_str(&r.string()?)?;
        // Nicotine+'s PeerInit.parse_network_message reads only the username and
        // connection-type strings and never consumes the trailing token
        // (slskmessages.py: it sets target_user and conn_type, then stops). The
        // legacy token is therefore effectively optional on the wire: read it
        // when present, default to 0 when absent, rather than requiring it.
        let token = if r.remaining() >= 4 { r.u32()? } else { 0 };
        Ok(PeerInit {
            username,
            connection_type,
            token,
        })
    }
}

/// A decoded peer-init message (the first frame on a new peer connection).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerInitMessage {
    PierceFirewall(PierceFirewall),
    PeerInit(PeerInit),
}

impl PeerInitMessage {
    /// Decodes a frame payload (u8 code + contents) as produced by
    /// [`crate::frame::split_frame`].
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(payload);
        match r.u8()? {
            code::PIERCE_FIREWALL => {
                Ok(PeerInitMessage::PierceFirewall(PierceFirewall::decode(&mut r)?))
            }
            code::PEER_INIT => Ok(PeerInitMessage::PeerInit(PeerInit::decode(&mut r)?)),
            other => Err(DecodeError::InvalidValue(format!(
                "unknown peer-init message code {other}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::split_frame;

    #[test]
    fn pierce_firewall_frame_is_byte_exact() {
        let frame = PierceFirewall { token: 42 }.to_frame();
        assert_eq!(
            frame,
            [
                0x05, 0x00, 0x00, 0x00, // length = code(1) + token(4)
                0x00, // code 0
                0x2A, 0x00, 0x00, 0x00, // token 42
            ]
        );
    }

    #[test]
    fn peer_init_round_trips_through_frame() {
        let original = PeerInit {
            username: "testuser".into(),
            connection_type: ConnectionType::Peer,
            token: 0,
        };
        let framed = original.to_frame();
        let (payload, rest) = split_frame(&framed).unwrap().unwrap();
        assert!(rest.is_empty());
        assert_eq!(
            PeerInitMessage::decode(payload).unwrap(),
            PeerInitMessage::PeerInit(original)
        );
    }

    #[test]
    fn unknown_connection_type_is_rejected() {
        let mut body = Vec::new();
        body.push(code::PEER_INIT);
        put_string(&mut body, "testuser");
        put_string(&mut body, "X");
        put_u32(&mut body, 0);
        assert!(matches!(
            PeerInitMessage::decode(&body),
            Err(DecodeError::InvalidValue(_))
        ));
    }

    #[test]
    fn peer_init_encode_is_byte_exact() {
        // Nicotine+ PeerInit.make_network_message (slskmessages.py) writes, in
        // order: pack_string(init_user), pack_string(conn_type), and
        // pack_uint32(0) — an *empty* token that is always zero in the modern
        // protocol. Pin the exact bytes so a field-order, width, or token
        // regression is caught directly.
        let frame = PeerInit {
            username: "user".into(),
            connection_type: ConnectionType::Peer,
            token: 0,
        }
        .to_frame();
        assert_eq!(
            frame,
            [
                0x12, 0x00, 0x00, 0x00, // length = code(1) + str(4+4) + str(4+1) + token(4) = 18
                0x01, // code 1
                0x04, 0x00, 0x00, 0x00, b'u', b's', b'e', b'r', // username "user"
                0x01, 0x00, 0x00, 0x00, b'P', // connection type "P"
                0x00, 0x00, 0x00, 0x00, // empty token (always 0)
            ]
        );
    }

    #[test]
    fn peer_init_decode_tolerates_absent_token() {
        // Nicotine+ PeerInit.parse_network_message reads only the two strings
        // and never consumes the token (slskmessages.py), so a frame that ends
        // right after the connection type must decode successfully with the
        // token defaulting to 0 — not error on a missing u32.
        let mut body = Vec::new();
        body.push(code::PEER_INIT);
        put_string(&mut body, "testuser");
        put_string(&mut body, "P");
        // deliberately no token field
        assert_eq!(
            PeerInitMessage::decode(&body).unwrap(),
            PeerInitMessage::PeerInit(PeerInit {
                username: "testuser".into(),
                connection_type: ConnectionType::Peer,
                token: 0,
            })
        );
    }

    #[test]
    fn peer_init_decode_reads_token_when_present() {
        // When the trailing token is present on the wire it is still read, so a
        // legacy non-zero token round-trips rather than being dropped.
        let mut body = Vec::new();
        body.push(code::PEER_INIT);
        put_string(&mut body, "testuser");
        put_string(&mut body, "P");
        put_u32(&mut body, 12345);
        assert_eq!(
            PeerInitMessage::decode(&body).unwrap(),
            PeerInitMessage::PeerInit(PeerInit {
                username: "testuser".into(),
                connection_type: ConnectionType::Peer,
                token: 12345,
            })
        );
    }

    #[test]
    fn peer_init_encode_always_emits_zero_token() {
        // Nicotine+ PeerInit.make_network_message writes `pack_uint32(0)` — a
        // literal constant, not the instance's token (slskmessages.py). So even
        // a PeerInit holding a non-zero legacy token (e.g. one read back off the
        // wire) must serialise a zero token, matching Nicotine+ exactly.
        let frame = PeerInit {
            username: "user".into(),
            connection_type: ConnectionType::Peer,
            token: 0xDEAD_BEEF,
        }
        .to_frame();
        assert_eq!(
            frame,
            [
                0x12, 0x00, 0x00, 0x00, // length = code(1) + str(4+4) + str(4+1) + token(4) = 18
                0x01, // code 1
                0x04, 0x00, 0x00, 0x00, b'u', b's', b'e', b'r', // username "user"
                0x01, 0x00, 0x00, 0x00, b'P', // connection type "P"
                0x00, 0x00, 0x00, 0x00, // token forced to 0 regardless of struct value
            ]
        );
    }

    #[test]
    fn peer_init_decode_requires_connection_type() {
        // Nicotine+ PeerInit.parse_network_message calls unpack_string() twice,
        // unconditionally — target_user then conn_type (slskmessages.py). The
        // connection-type string is therefore mandatory, unlike the trailing
        // legacy token which it never reads. A frame ending right after the
        // username must error, not decode with a defaulted connection type.
        let mut body = Vec::new();
        body.push(code::PEER_INIT);
        put_string(&mut body, "testuser");
        // deliberately no connection-type string
        assert!(matches!(
            PeerInitMessage::decode(&body),
            Err(DecodeError::UnexpectedEof { .. })
        ));
    }

    #[test]
    fn pierce_firewall_decode_requires_full_token() {
        // Nicotine+ PierceFireWall.parse_network_message is a single
        // unpack_uint32() (slskmessages.py): the 4-byte token is mandatory. A
        // body carrying fewer than four token bytes is truncated and must error
        // rather than silently reading a short/zero token.
        let body = [code::PIERCE_FIREWALL, 0x01, 0x02]; // only 2 of 4 token bytes
        assert!(matches!(
            PeerInitMessage::decode(&body),
            Err(DecodeError::UnexpectedEof { .. })
        ));
    }

    #[test]
    fn file_and_distributed_connection_types_round_trip() {
        // Nicotine+ uses the single-character connection-type strings "F" (file
        // transfer) and "D" (distributed network) alongside "P" (peer); pin
        // that both encode and decode through a full frame, not just "P".
        for conn_type in [ConnectionType::File, ConnectionType::Distributed] {
            let original = PeerInit {
                username: "testuser".into(),
                connection_type: conn_type,
                token: 0,
            };
            let framed = original.to_frame();
            let (payload, rest) = split_frame(&framed).unwrap().unwrap();
            assert!(rest.is_empty());
            assert_eq!(
                PeerInitMessage::decode(payload).unwrap(),
                PeerInitMessage::PeerInit(original)
            );
        }
    }

    #[test]
    fn pierce_firewall_decodes_through_peer_init_message() {
        // Nicotine+ PierceFireWall.parse_network_message is a single
        // unpack_uint32() for the token taken from the ConnectToPeer message
        // (slskmessages.py). Decode a real framed PierceFirewall and confirm the
        // token comes back intact.
        let framed = PierceFirewall { token: 0xDEAD_BEEF }.to_frame();
        let (payload, rest) = split_frame(&framed).unwrap().unwrap();
        assert!(rest.is_empty());
        assert_eq!(
            PeerInitMessage::decode(payload).unwrap(),
            PeerInitMessage::PierceFirewall(PierceFirewall { token: 0xDEAD_BEEF })
        );
    }

    #[test]
    fn unknown_message_code_is_rejected() {
        // Nicotine+ dispatches peer-init frames only when the u8 code is in
        // PEER_INIT_MESSAGE_CLASSES, which maps exactly {0: PierceFireWall,
        // 1: PeerInit} (slskmessages.py); any other code is not a peer-init
        // message. Our decoder rejects such codes rather than misparsing them.
        let body = [0x02u8, 0x00, 0x00, 0x00, 0x00]; // code 2, then a u32
        assert!(matches!(
            PeerInitMessage::decode(&body),
            Err(DecodeError::InvalidValue(_))
        ));
    }
}
