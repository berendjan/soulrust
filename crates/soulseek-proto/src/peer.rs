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
        put_u32(&mut body, self.token);
        frame_u8(code::PEER_INIT, &body)
    }

    pub fn decode(r: &mut Reader) -> Result<Self, DecodeError> {
        Ok(PeerInit {
            username: r.string()?,
            connection_type: ConnectionType::from_str(&r.string()?)?,
            token: r.u32()?,
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
}
