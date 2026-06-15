//! Message framing.
//!
//! Every Soulseek message is `[u32 length][payload]` where `length` is the
//! payload size (message code + contents). This module is transport-agnostic:
//! [`split_frame`] peels complete frames off an accumulating receive buffer,
//! and the `frame_*` helpers wrap an encoded body for sending.

use std::fmt;

use crate::wire::{put_u32, put_u8};

/// Upper bound accepted for a single frame. Generous (server messages such
/// as room lists can run to several megabytes) while still rejecting
/// nonsense lengths from a corrupt or hostile stream before allocation.
pub const MAX_FRAME_LEN: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// Declared length exceeds [`MAX_FRAME_LEN`].
    Oversized { declared: usize },
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::Oversized { declared } => {
                write!(f, "frame length {declared} exceeds maximum {MAX_FRAME_LEN}")
            }
        }
    }
}

impl std::error::Error for FrameError {}

/// Splits one complete frame off the front of `buf`.
///
/// Returns `Ok(None)` if the buffer does not yet hold a complete frame
/// (read more bytes and call again), or `Ok(Some((payload, rest)))` where
/// `payload` is the frame body (code + contents, without the length prefix)
/// and `rest` is the unconsumed remainder of the buffer.
pub fn split_frame(buf: &[u8]) -> Result<Option<(&[u8], &[u8])>, FrameError> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let declared = u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;
    if declared > MAX_FRAME_LEN {
        return Err(FrameError::Oversized { declared });
    }
    if buf.len() < 4 + declared {
        return Ok(None);
    }
    Ok(Some((&buf[4..4 + declared], &buf[4 + declared..])))
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
    fn rejects_oversized_frames() {
        let mut buf = Vec::new();
        put_u32(&mut buf, (MAX_FRAME_LEN + 1) as u32);
        assert_eq!(
            split_frame(&buf),
            Err(FrameError::Oversized { declared: MAX_FRAME_LEN + 1 })
        );
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
