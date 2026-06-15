//! Message framing.
//!
//! Every Soulseek message is `[u32 length][payload]` where `length` is the
//! payload size (message code + contents). This module is transport-agnostic:
//! [`split_frame`] peels complete frames off an accumulating receive buffer,
//! and the `frame_*` helpers wrap an encoded body for sending.

use std::fmt;

use crate::wire::{put_u32, put_u8};

/// Per-connection-type caps on a single declared frame length, mirroring
/// Nicotine+'s incoming-message-size limits (slskproto.py). The right cap to
/// apply is a property of the *connection*, so the reading edge passes its
/// limit to [`split_frame_capped`]; anything larger is nonsense from a corrupt
/// or hostile stream and is rejected before we wait to buffer it, exactly as
/// Nicotine+ closes the connection on `msg_size > MAX_INCOMING_MESSAGE_SIZE_*`.
///
/// - Server connections carry large room/share data — `MAX_INCOMING_MESSAGE_SIZE_LARGE`.
/// - Browse and user-info peer responses are likewise large.
/// - Other peer messages are medium — `MAX_INCOMING_MESSAGE_SIZE_MEDIUM`.
/// - Peer-init handshakes and distributed-network messages are tiny —
///   `MAX_INCOMING_MESSAGE_SIZE_SMALL`.
pub const MAX_SERVER_MESSAGE_LEN: usize = 469_762_048; // 448 MiB (Nicotine+ LARGE)
/// Large peer responses (SharedFileListResponse, UserInfoResponse).
pub const MAX_LARGE_PEER_MESSAGE_LEN: usize = 469_762_048; // 448 MiB (LARGE)
/// General peer messages.
pub const MAX_PEER_MESSAGE_LEN: usize = 16 * 1024 * 1024; // 16 MiB (MEDIUM)
/// Peer-init handshakes and distributed-network messages.
pub const MAX_PEER_INIT_MESSAGE_LEN: usize = 16 * 1024; // 16 KiB (SMALL)

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// Declared length exceeds the cap for the connection it arrived on.
    Oversized { declared: usize, max: usize },
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::Oversized { declared, max } => {
                write!(f, "frame length {declared} exceeds this connection's maximum {max}")
            }
        }
    }
}

impl std::error::Error for FrameError {}

/// Splits one complete frame off the front of `buf`, rejecting a declared
/// length greater than `max_len` (the cap for this connection type).
///
/// Returns `Ok(None)` if the buffer does not yet hold a complete frame
/// (read more bytes and call again), or `Ok(Some((payload, rest)))` where
/// `payload` is the frame body (code + contents, without the length prefix)
/// and `rest` is the unconsumed remainder of the buffer.
pub fn split_frame_capped(buf: &[u8], max_len: usize) -> Result<Option<(&[u8], &[u8])>, FrameError> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let declared = u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;
    if declared > max_len {
        return Err(FrameError::Oversized { declared, max: max_len });
    }
    if buf.len() < 4 + declared {
        return Ok(None);
    }
    Ok(Some((&buf[4..4 + declared], &buf[4 + declared..])))
}

/// Splits one complete frame, capped at the most permissive limit
/// ([`MAX_SERVER_MESSAGE_LEN`]). Convenience for callers that aren't a
/// connection edge (tests, helpers); real readers should use
/// [`split_frame_capped`] with their connection's limit.
pub fn split_frame(buf: &[u8]) -> Result<Option<(&[u8], &[u8])>, FrameError> {
    split_frame_capped(buf, MAX_SERVER_MESSAGE_LEN)
}

/// Frames a message with a `u32` code (server and peer connections).
pub fn frame_u32(code: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + body.len());
    put_u32(&mut out, (4 + body.len()) as u32);
    put_u32(&mut out, code);
    out.extend_from_slice(body);
    out
}

/// Frames a message with a `u8` code (peer-init and distributed connections).
pub fn frame_u8(code: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + body.len());
    put_u32(&mut out, (1 + body.len()) as u32);
    put_u8(&mut out, code);
    out.extend_from_slice(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incomplete_frames_request_more_data() {
        assert_eq!(split_frame(&[]), Ok(None));
        assert_eq!(split_frame(&[0x08, 0x00, 0x00]), Ok(None)); // partial length
        assert_eq!(split_frame(&[0x08, 0x00, 0x00, 0x00, 0xAA]), Ok(None)); // partial payload
    }

    #[test]
    fn splits_frame_and_leaves_remainder() {
        // One 2-byte frame followed by the start of the next frame.
        let buf = [0x02, 0x00, 0x00, 0x00, 0xAA, 0xBB, 0x05, 0x00];
        let (payload, rest) = split_frame(&buf).unwrap().unwrap();
        assert_eq!(payload, [0xAA, 0xBB]);
        assert_eq!(rest, [0x05, 0x00]);
    }

    #[test]
    fn rejects_frames_over_the_connection_cap() {
        // A peer-init connection caps at MAX_PEER_INIT_MESSAGE_LEN (16 KiB); a
        // larger declared length is rejected, while the same frame is fine on a
        // server connection (the cap is per connection type).
        let mut buf = Vec::new();
        put_u32(&mut buf, (MAX_PEER_INIT_MESSAGE_LEN + 1) as u32);
        assert_eq!(
            split_frame_capped(&buf, MAX_PEER_INIT_MESSAGE_LEN),
            Err(FrameError::Oversized {
                declared: MAX_PEER_INIT_MESSAGE_LEN + 1,
                max: MAX_PEER_INIT_MESSAGE_LEN,
            })
        );
        // Well under the server cap → not oversized, just awaiting the body.
        assert_eq!(split_frame_capped(&buf, MAX_SERVER_MESSAGE_LEN), Ok(None));
    }

    #[test]
    fn accepts_frame_at_max_declared_length() {
        // Nicotine+ closes the connection only when `msg_size > MAX_INCOMING_
        // MESSAGE_SIZE_*` (strictly greater, slskproto.py), so a declared length
        // exactly equal to the limit must NOT be rejected. We only have the
        // length prefix here, so it asks for more bytes rather than erroring —
        // proving the boundary value is accepted, not oversized.
        let mut buf = Vec::new();
        put_u32(&mut buf, MAX_PEER_INIT_MESSAGE_LEN as u32);
        assert_eq!(split_frame_capped(&buf, MAX_PEER_INIT_MESSAGE_LEN), Ok(None));
    }

    #[test]
    fn zero_length_frame_yields_empty_payload() {
        // A declared length of 0 is well-formed framing: it consumes just the
        // 4-byte prefix and produces an empty payload. Nicotine+ computes
        // `msg_size_total = msg_size + 4`, so a zero msg_size advances the
        // buffer by exactly 4 bytes (slskproto.py).
        let buf = [0x00, 0x00, 0x00, 0x00, 0xAA, 0xBB];
        let (payload, rest) = split_frame(&buf).unwrap().unwrap();
        assert_eq!(payload, [] as [u8; 0]);
        assert_eq!(rest, [0xAA, 0xBB]);
    }

    #[test]
    fn frame_u32_layout() {
        let framed = frame_u32(2, &[0xBA, 0x08, 0x00, 0x00]);
        assert_eq!(
            framed,
            [
                0x08, 0x00, 0x00, 0x00, // length = code(4) + body(4)
                0x02, 0x00, 0x00, 0x00, // code 2
                0xBA, 0x08, 0x00, 0x00, // body
            ]
        );
    }

    #[test]
    fn frame_u8_layout() {
        let framed = frame_u8(0, &[0x2A, 0x00, 0x00, 0x00]);
        assert_eq!(
            framed,
            [
                0x05, 0x00, 0x00, 0x00, // length = code(1) + body(4)
                0x00, // code 0
                0x2A, 0x00, 0x00, 0x00, // body
            ]
        );
    }
}
