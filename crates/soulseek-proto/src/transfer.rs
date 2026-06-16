//! File-transfer messages.
//!
//! Two wire shapes live here:
//!
//! - **Peer messages** (`u32` code, like the rest of [`crate::peer_message`]):
//!   the transfer *negotiation* exchanged on a normal `P` connection —
//!   `QueueUpload`, `TransferRequest`/`TransferResponse`,
//!   `PlaceInQueueRequest`/`PlaceInQueueResponse`, `UploadDenied`,
//!   `UploadFailed`.
//! - **File messages** ([`FileTransferInit`], [`FileOffset`]): the first bytes
//!   on an `F` connection. These are **not** length-prefixed and carry no code —
//!   a bare `u32` token then a bare `u64` offset, after which the connection is
//!   raw file bytes. Mirrors Nicotine+'s `FileMessage` handling in
//!   `slskproto.py` (`_process_file_init_message` / `_process_file_offset_message`).
//!
//! Bytes never cross the message bus: the reactor streams file contents
//! straight to/from disk; only these control messages and progress events do.

use crate::frame::frame_u32;
use crate::wire::{put_bool, put_string, put_u32, put_u64, DecodeError, Reader};

pub mod code {
    pub const TRANSFER_REQUEST: u32 = 40;
    pub const TRANSFER_RESPONSE: u32 = 41;
    pub const QUEUE_UPLOAD: u32 = 43;
    pub const PLACE_IN_QUEUE_RESPONSE: u32 = 44;
    pub const UPLOAD_FAILED: u32 = 46;
    pub const UPLOAD_DENIED: u32 = 50;
    pub const PLACE_IN_QUEUE_REQUEST: u32 = 51;
}

/// Which way a [`TransferRequest`] goes, as seen by its sender. Nicotine+'s
/// `TransferDirection` (`slskmessages.py`): `DOWNLOAD = 0`, `UPLOAD = 1`. Only
/// the `Upload` form carries a file size on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    Download,
    Upload,
}

impl TransferDirection {
    pub fn as_u32(self) -> u32 {
        match self {
            TransferDirection::Download => 0,
            TransferDirection::Upload => 1,
        }
    }

    fn from_u32(value: u32) -> Result<Self, DecodeError> {
        match value {
            0 => Ok(TransferDirection::Download),
            1 => Ok(TransferDirection::Upload),
            other => Err(DecodeError::InvalidValue(format!("unknown transfer direction {other}"))),
        }
    }
}

/// Peer code 40 — TransferRequest: the uploader announces it is ready to send a
/// file (direction `Upload`, carrying the size), or, in the legacy form, a
/// download request (direction `Download`, no size).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferRequest {
    pub direction: TransferDirection,
    pub token: u32,
    pub file: String,
    /// Present (and required) only for `Upload`; `None` for `Download`.
    pub filesize: Option<u64>,
}

impl TransferRequest {
    pub fn to_frame(&self) -> Vec<u8> {
        let mut body = Vec::new();
        put_u32(&mut body, self.direction.as_u32());
        put_u32(&mut body, self.token);
        put_string(&mut body, &self.file);
        if self.direction == TransferDirection::Upload {
            put_u64(&mut body, self.filesize.unwrap_or(0));
        }
        frame_u32(code::TRANSFER_REQUEST, &body)
    }

    pub fn decode(r: &mut Reader) -> Result<Self, DecodeError> {
        let direction = TransferDirection::from_u32(r.u32()?)?;
        let token = r.u32()?;
        let file = r.string()?;
        let filesize = match direction {
            TransferDirection::Upload => Some(r.u64()?),
            TransferDirection::Download => None,
        };
        Ok(TransferRequest { direction, token, file, filesize })
    }
}

/// Peer code 41 — TransferResponse: accept (`allowed`, with the size echoed) or
/// reject (with a reason) a [`TransferRequest`]. Nicotine+ stops parsing right
/// after `allowed` when nothing follows, so both trailing fields are optional.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferResponse {
    pub token: u32,
    pub allowed: bool,
    /// Set when `allowed`; the file size echoed back.
    pub filesize: Option<u64>,
    /// Set when not `allowed`; e.g. `"Queued"` or `"Cancelled"`.
    pub reason: Option<String>,
}

impl TransferResponse {
    pub fn to_frame(&self) -> Vec<u8> {
        let mut body = Vec::new();
        put_u32(&mut body, self.token);
        put_bool(&mut body, self.allowed);
        // Nicotine+ decodes a filesize iff allowed, otherwise a reason; encode to
        // match that, so the two are never both present on the wire.
        if self.allowed {
            if let Some(filesize) = self.filesize {
                put_u64(&mut body, filesize);
            }
        } else if let Some(reason) = &self.reason {
            put_string(&mut body, reason);
        }
        frame_u32(code::TRANSFER_RESPONSE, &body)
    }

    pub fn decode(r: &mut Reader) -> Result<Self, DecodeError> {
        let token = r.u32()?;
        let allowed = r.bool()?;
        let (filesize, reason) = if r.is_empty() {
            (None, None)
        } else if allowed {
            (Some(r.u64()?), None)
        } else {
            (None, Some(r.string()?))
        };
        Ok(TransferResponse { token, allowed, filesize, reason })
    }
}

/// Peer code 43 — QueueUpload: the downloader asks the uploader to queue a file
/// for transfer (the modern request path; the uploader replies with its own
/// `TransferRequest` when a slot frees).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueUpload {
    pub file: String,
}

impl QueueUpload {
    pub fn to_frame(&self) -> Vec<u8> {
        let mut body = Vec::new();
        put_string(&mut body, &self.file);
        frame_u32(code::QUEUE_UPLOAD, &body)
    }

    pub fn decode(r: &mut Reader) -> Result<Self, DecodeError> {
        Ok(QueueUpload { file: r.string()? })
    }
}

/// Peer code 51 — PlaceInQueueRequest: the downloader asks where its queued file
/// sits in the uploader's queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaceInQueueRequest {
    pub file: String,
}

impl PlaceInQueueRequest {
    pub fn to_frame(&self) -> Vec<u8> {
        let mut body = Vec::new();
        put_string(&mut body, &self.file);
        frame_u32(code::PLACE_IN_QUEUE_REQUEST, &body)
    }

    pub fn decode(r: &mut Reader) -> Result<Self, DecodeError> {
        Ok(PlaceInQueueRequest { file: r.string()? })
    }
}

/// Peer code 44 — PlaceInQueueResponse: the uploader's answer to
/// [`PlaceInQueueRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaceInQueueResponse {
    pub filename: String,
    pub place: u32,
}

impl PlaceInQueueResponse {
    pub fn to_frame(&self) -> Vec<u8> {
        let mut body = Vec::new();
        put_string(&mut body, &self.filename);
        put_u32(&mut body, self.place);
        frame_u32(code::PLACE_IN_QUEUE_RESPONSE, &body)
    }

    pub fn decode(r: &mut Reader) -> Result<Self, DecodeError> {
        Ok(PlaceInQueueResponse { filename: r.string()?, place: r.u32()? })
    }
}

/// Peer code 50 — UploadDenied: the uploader refuses a queued file (e.g. it is
/// no longer shared), with a reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadDenied {
    pub file: String,
    pub reason: String,
}

impl UploadDenied {
    pub fn to_frame(&self) -> Vec<u8> {
        let mut body = Vec::new();
        put_string(&mut body, &self.file);
        put_string(&mut body, &self.reason);
        frame_u32(code::UPLOAD_DENIED, &body)
    }

    pub fn decode(r: &mut Reader) -> Result<Self, DecodeError> {
        Ok(UploadDenied { file: r.string()?, reason: r.string()? })
    }
}

/// Peer code 46 — UploadFailed: the uploader reports a transfer of `file` failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadFailed {
    pub file: String,
}

impl UploadFailed {
    pub fn to_frame(&self) -> Vec<u8> {
        let mut body = Vec::new();
        put_string(&mut body, &self.file);
        frame_u32(code::UPLOAD_FAILED, &body)
    }

    pub fn decode(r: &mut Reader) -> Result<Self, DecodeError> {
        Ok(UploadFailed { file: r.string()? })
    }
}

/// The first message on an `F` (file) connection: a bare `u32` token (no length
/// prefix, no code) correlating the connection to a prior [`TransferRequest`].
/// Sent by the uploader. Nicotine+ `FileTransferInit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileTransferInit {
    pub token: u32,
}

impl FileTransferInit {
    /// Wire length: a bare little-endian `u32`.
    pub const LEN: usize = 4;

    pub fn to_bytes(&self) -> [u8; 4] {
        self.token.to_le_bytes()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let raw: [u8; 4] = bytes.get(..4).and_then(|b| b.try_into().ok()).ok_or(
            DecodeError::UnexpectedEof { needed: 4, remaining: bytes.len() },
        )?;
        Ok(FileTransferInit { token: u32::from_le_bytes(raw) })
    }
}

/// Sent by the downloader on an `F` connection right after [`FileTransferInit`]:
/// a bare `u64` byte offset to resume from (`0` for a fresh transfer). After
/// this, the uploader streams raw file bytes. Nicotine+ `FileOffset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileOffset {
    pub offset: u64,
}

impl FileOffset {
    /// Wire length: a bare little-endian `u64`.
    pub const LEN: usize = 8;

    pub fn to_bytes(&self) -> [u8; 8] {
        self.offset.to_le_bytes()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let raw: [u8; 8] = bytes.get(..8).and_then(|b| b.try_into().ok()).ok_or(
            DecodeError::UnexpectedEof { needed: 8, remaining: bytes.len() },
        )?;
        Ok(FileOffset { offset: u64::from_le_bytes(raw) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::split_frame;

    fn decode_peer(frame: &[u8]) -> (u32, Vec<u8>) {
        let (payload, rest) = split_frame(frame).unwrap().unwrap();
        assert!(rest.is_empty());
        let code = u32::from_le_bytes(payload[..4].try_into().unwrap());
        (code, payload[4..].to_vec())
    }

    #[test]
    fn transfer_request_upload_round_trips_with_filesize() {
        let request = TransferRequest {
            direction: TransferDirection::Upload,
            token: 0xABCD,
            file: "Music\\song.mp3".into(),
            filesize: Some(5_242_880),
        };
        let (code, body) = decode_peer(&request.to_frame());
        assert_eq!(code, code::TRANSFER_REQUEST);
        assert_eq!(TransferRequest::decode(&mut Reader::new(&body)).unwrap(), request);
    }

    #[test]
    fn transfer_request_download_has_no_filesize() {
        let request = TransferRequest {
            direction: TransferDirection::Download,
            token: 7,
            file: "x".into(),
            filesize: None,
        };
        let (_, body) = decode_peer(&request.to_frame());
        // direction(4) + token(4) + string(4+1) = 13 bytes, no trailing size.
        assert_eq!(body.len(), 13);
        assert_eq!(TransferRequest::decode(&mut Reader::new(&body)).unwrap(), request);
    }

    #[test]
    fn transfer_response_allowed_carries_filesize() {
        let response =
            TransferResponse { token: 9, allowed: true, filesize: Some(4096), reason: None };
        let (code, body) = decode_peer(&response.to_frame());
        assert_eq!(code, code::TRANSFER_RESPONSE);
        assert_eq!(TransferResponse::decode(&mut Reader::new(&body)).unwrap(), response);
    }

    #[test]
    fn transfer_response_rejected_carries_reason() {
        let response = TransferResponse {
            token: 9,
            allowed: false,
            filesize: None,
            reason: Some("Queued".into()),
        };
        let (_, body) = decode_peer(&response.to_frame());
        assert_eq!(TransferResponse::decode(&mut Reader::new(&body)).unwrap(), response);
    }

    #[test]
    fn transfer_response_bare_allowed_decodes_with_no_trailing_fields() {
        // token + allowed and nothing else — Nicotine+ returns early.
        let mut body = Vec::new();
        put_u32(&mut body, 5);
        put_bool(&mut body, true);
        let decoded = TransferResponse::decode(&mut Reader::new(&body)).unwrap();
        assert_eq!(decoded, TransferResponse { token: 5, allowed: true, filesize: None, reason: None });
    }

    #[test]
    fn transfer_response_bare_rejected_decodes_with_no_reason() {
        // token + allowed=false and nothing else. Nicotine+'s
        // TransferResponse.parse_network_message returns early at
        // `if not self.has_remaining_content(): return` BEFORE the
        // `else: self.reason = self.unpack_string()` branch, so a rejected
        // response with no trailing reason leaves reason unset — the symmetric
        // case to the bare-allowed early return above.
        let mut body = Vec::new();
        put_u32(&mut body, 5);
        put_bool(&mut body, false);
        let decoded = TransferResponse::decode(&mut Reader::new(&body)).unwrap();
        assert_eq!(
            decoded,
            TransferResponse { token: 5, allowed: false, filesize: None, reason: None }
        );
    }

    #[test]
    fn queue_upload_round_trips() {
        let msg = QueueUpload { file: "Music\\song.mp3".into() };
        let (code, body) = decode_peer(&msg.to_frame());
        assert_eq!(code, code::QUEUE_UPLOAD);
        assert_eq!(QueueUpload::decode(&mut Reader::new(&body)).unwrap(), msg);
    }

    #[test]
    fn place_in_queue_messages_round_trip() {
        let request = PlaceInQueueRequest { file: "a\\b.mp3".into() };
        let (code, body) = decode_peer(&request.to_frame());
        assert_eq!(code, code::PLACE_IN_QUEUE_REQUEST);
        assert_eq!(PlaceInQueueRequest::decode(&mut Reader::new(&body)).unwrap(), request);

        let response = PlaceInQueueResponse { filename: "a\\b.mp3".into(), place: 3 };
        let (code, body) = decode_peer(&response.to_frame());
        assert_eq!(code, code::PLACE_IN_QUEUE_RESPONSE);
        assert_eq!(PlaceInQueueResponse::decode(&mut Reader::new(&body)).unwrap(), response);
    }

    #[test]
    fn upload_denied_and_failed_round_trip() {
        let denied = UploadDenied { file: "a.mp3".into(), reason: "Not shared".into() };
        let (code, body) = decode_peer(&denied.to_frame());
        assert_eq!(code, code::UPLOAD_DENIED);
        assert_eq!(UploadDenied::decode(&mut Reader::new(&body)).unwrap(), denied);

        let failed = UploadFailed { file: "a.mp3".into() };
        let (code, body) = decode_peer(&failed.to_frame());
        assert_eq!(code, code::UPLOAD_FAILED);
        assert_eq!(UploadFailed::decode(&mut Reader::new(&body)).unwrap(), failed);
    }

    #[test]
    fn file_messages_are_bare_little_endian_scalars() {
        // F-connection messages carry no length prefix and no code.
        assert_eq!(FileTransferInit { token: 0x04030201 }.to_bytes(), [0x01, 0x02, 0x03, 0x04]);
        assert_eq!(FileTransferInit::decode(&[0x01, 0x02, 0x03, 0x04]).unwrap().token, 0x04030201);
        assert_eq!(FileOffset { offset: 1 }.to_bytes(), [1, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(FileOffset::decode(&[1, 0, 0, 0, 0, 0, 0, 0]).unwrap().offset, 1);
        // Too-short buffers fault rather than panic.
        assert!(FileTransferInit::decode(&[0x01, 0x02]).is_err());
        assert!(FileOffset::decode(&[0x01]).is_err());
    }
}
