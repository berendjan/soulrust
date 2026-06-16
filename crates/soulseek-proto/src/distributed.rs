//! Distributed-network messages, exchanged on `D` connections.
//!
//! The distributed search network is a tree: each peer adopts a *parent* and
//! relays search requests down to its *children*. Like peer-init messages these
//! use a **`u8` message code** (frame is `[u32 len][u8 code][body]`), unlike the
//! `u32`-code server and peer messages.
//!
//! Mirrors Nicotine+'s `DistribMessage` subclasses (`slskmessages.py`).

use crate::frame::frame_u8;
use crate::wire::{put_string, put_u32, put_u8, DecodeError, Reader};

pub mod code {
    pub const PING: u8 = 0;
    pub const SEARCH: u8 = 3;
    pub const BRANCH_LEVEL: u8 = 4;
    pub const BRANCH_ROOT: u8 = 5;
    pub const CHILD_DEPTH: u8 = 7;
    pub const EMBEDDED: u8 = 93;
}

/// The `identifier` codepoint a [`DistribSearch`] must carry — ASCII 1.
/// Nicotine+ rejects any other value.
pub const SEARCH_IDENTIFIER: u32 = 1;

/// Distrib code 0 — a keepalive ping. No body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DistribPing;

impl DistribPing {
    pub fn to_frame(&self) -> Vec<u8> {
        frame_u8(code::PING, &[])
    }
}

/// Distrib code 3 — a search relayed down the distributed tree. We match it
/// against our shares (like a server search) and forward it to our children.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DistribSearch {
    /// Always [`SEARCH_IDENTIFIER`] (ASCII 1) on the wire.
    pub identifier: u32,
    pub username: String,
    pub token: u32,
    pub query: String,
}

impl DistribSearch {
    pub fn to_frame(&self) -> Vec<u8> {
        let mut body = Vec::new();
        put_u32(&mut body, self.identifier);
        put_string(&mut body, &self.username);
        put_u32(&mut body, self.token);
        put_string(&mut body, &self.query);
        frame_u8(code::SEARCH, &body)
    }

    pub fn decode(r: &mut Reader) -> Result<Self, DecodeError> {
        Ok(DistribSearch {
            identifier: r.u32()?,
            username: r.string()?,
            token: r.u32()?,
            query: r.string()?,
        })
    }
}

/// Distrib code 4 — our (or our parent's) position in the tree, as a signed
/// depth. Nicotine+ uses `pack_int32`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DistribBranchLevel {
    pub level: i32,
}

impl DistribBranchLevel {
    pub fn to_frame(&self) -> Vec<u8> {
        let mut body = Vec::new();
        put_u32(&mut body, self.level as u32); // little-endian i32 == u32 bits
        frame_u8(code::BRANCH_LEVEL, &body)
    }

    pub fn decode(r: &mut Reader) -> Result<Self, DecodeError> {
        Ok(DistribBranchLevel { level: r.u32()? as i32 })
    }
}

/// Distrib code 5 — the username at the root of our branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DistribBranchRoot {
    pub root_username: String,
}

impl DistribBranchRoot {
    pub fn to_frame(&self) -> Vec<u8> {
        let mut body = Vec::new();
        put_string(&mut body, &self.root_username);
        frame_u8(code::BRANCH_ROOT, &body)
    }

    pub fn decode(r: &mut Reader) -> Result<Self, DecodeError> {
        Ok(DistribBranchRoot { root_username: r.string()? })
    }
}

/// Distrib code 7 — the depth of our subtree, reported to our parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DistribChildDepth {
    pub depth: u32,
}

impl DistribChildDepth {
    pub fn to_frame(&self) -> Vec<u8> {
        let mut body = Vec::new();
        put_u32(&mut body, self.depth);
        frame_u8(code::CHILD_DEPTH, &body)
    }

    pub fn decode(r: &mut Reader) -> Result<Self, DecodeError> {
        Ok(DistribChildDepth { depth: r.u32()? })
    }
}

/// Distrib code 93 — a distributed message embedded by the server: a `u8`
/// inner code followed by that message's raw body (in practice a
/// [`DistribSearch`]). We keep the inner bytes to decode or forward.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DistribEmbedded {
    pub inner_code: u8,
    pub inner_message: Vec<u8>,
}

impl DistribEmbedded {
    pub fn to_frame(&self) -> Vec<u8> {
        let mut body = Vec::new();
        put_u8(&mut body, self.inner_code);
        body.extend_from_slice(&self.inner_message);
        frame_u8(code::EMBEDDED, &body)
    }

    pub fn decode(r: &mut Reader) -> Result<Self, DecodeError> {
        let inner_code = r.u8()?;
        let inner_message = r.rest().to_vec();
        Ok(DistribEmbedded { inner_code, inner_message })
    }
}

/// A decoded distributed message. Unknown codes are preserved, like the server
/// and peer message enums.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DistributedMessage {
    Ping,
    Search(DistribSearch),
    BranchLevel(DistribBranchLevel),
    BranchRoot(DistribBranchRoot),
    ChildDepth(DistribChildDepth),
    Embedded(DistribEmbedded),
    Unknown { code: u8, body: Vec<u8> },
}

impl DistributedMessage {
    /// Decodes a frame payload (`[u8 code][body]`) as produced by
    /// [`crate::frame::split_frame`].
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(payload);
        let code = r.u8()?;
        match code {
            code::PING => Ok(DistributedMessage::Ping),
            code::SEARCH => Ok(DistributedMessage::Search(DistribSearch::decode(&mut r)?)),
            code::BRANCH_LEVEL => {
                Ok(DistributedMessage::BranchLevel(DistribBranchLevel::decode(&mut r)?))
            }
            code::BRANCH_ROOT => {
                Ok(DistributedMessage::BranchRoot(DistribBranchRoot::decode(&mut r)?))
            }
            code::CHILD_DEPTH => {
                Ok(DistributedMessage::ChildDepth(DistribChildDepth::decode(&mut r)?))
            }
            code::EMBEDDED => Ok(DistributedMessage::Embedded(DistribEmbedded::decode(&mut r)?)),
            _ => Ok(DistributedMessage::Unknown { code, body: r.rest().to_vec() }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::split_frame;

    fn decode_frame(frame: &[u8]) -> DistributedMessage {
        let (payload, rest) = split_frame(frame).unwrap().unwrap();
        assert!(rest.is_empty());
        DistributedMessage::decode(payload).unwrap()
    }

    #[test]
    fn ping_is_a_bare_code() {
        assert_eq!(DistribPing.to_frame(), [0x01, 0x00, 0x00, 0x00, 0x00]); // len 1, code 0
        assert_eq!(decode_frame(&DistribPing.to_frame()), DistributedMessage::Ping);
    }

    #[test]
    fn search_round_trips() {
        let search = DistribSearch {
            identifier: SEARCH_IDENTIFIER,
            username: "bob".into(),
            token: 0xABCD,
            query: "deep purple".into(),
        };
        assert_eq!(decode_frame(&search.to_frame()), DistributedMessage::Search(search));
    }

    #[test]
    fn branch_level_round_trips_including_negative() {
        for level in [0i32, 3, -1] {
            let msg = DistribBranchLevel { level };
            assert_eq!(decode_frame(&msg.to_frame()), DistributedMessage::BranchLevel(msg));
        }
    }

    #[test]
    fn branch_root_and_child_depth_round_trip() {
        let root = DistribBranchRoot { root_username: "alice".into() };
        assert_eq!(decode_frame(&root.to_frame()), DistributedMessage::BranchRoot(root));

        let depth = DistribChildDepth { depth: 5 };
        assert_eq!(decode_frame(&depth.to_frame()), DistributedMessage::ChildDepth(depth));
    }

    #[test]
    fn embedded_carries_an_inner_search() {
        // The server embeds a DistribSearch (inner code 3) in a code-93 message.
        let inner = DistribSearch {
            identifier: SEARCH_IDENTIFIER,
            username: "carol".into(),
            token: 7,
            query: "jazz".into(),
        };
        // The embedded inner body is the DistribSearch *body* (no frame header).
        let inner_body = &inner.to_frame()[5..]; // strip [u32 len][u8 code]
        let embedded = DistribEmbedded { inner_code: code::SEARCH, inner_message: inner_body.to_vec() };

        let DistributedMessage::Embedded(decoded) = decode_frame(&embedded.to_frame()) else {
            panic!("expected embedded");
        };
        assert_eq!(decoded.inner_code, code::SEARCH);
        // The inner bytes decode back to the original search.
        assert_eq!(DistribSearch::decode(&mut Reader::new(&decoded.inner_message)).unwrap(), inner);
    }

    #[test]
    fn unknown_distrib_code_is_preserved() {
        let frame = frame_u8(99, &[1, 2, 3]);
        let (payload, _) = split_frame(&frame).unwrap().unwrap();
        assert_eq!(
            DistributedMessage::decode(payload).unwrap(),
            DistributedMessage::Unknown { code: 99, body: vec![1, 2, 3] }
        );
    }
}
