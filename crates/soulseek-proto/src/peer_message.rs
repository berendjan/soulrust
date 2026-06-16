//! Peer messages: exchanged over an established peer-to-peer connection
//! (after the peer-init handshake in [`crate::peer`]). Like server messages
//! these use a `u32` code, but the bodies differ — most notably the shared
//! file list, whose contents are zlib-compressed on the wire.
//!
//! This module covers the "browse a user's shares" exchange:
//! `GetSharedFileList` (code 4) → `SharedFileListResponse` (code 5).

use std::io::Read;

use crate::frame::frame_u32;
use crate::wire::{put_bool, put_string, put_u32, put_u64, put_u8, DecodeError, Reader};

pub mod code {
    pub const GET_SHARED_FILE_LIST: u32 = 4;
    pub const SHARED_FILE_LIST: u32 = 5;
    pub const FILE_SEARCH_RESPONSE: u32 = 9;
    pub const USER_INFO_REQUEST: u32 = 15;
    pub const USER_INFO_RESPONSE: u32 = 16;
    pub const FOLDER_CONTENTS_REQUEST: u32 = 36;
    pub const FOLDER_CONTENTS_RESPONSE: u32 = 37;
}

/// Upper bound on a *decompressed* shared file list. A hostile peer can send a
/// small zlib stream that inflates to gigabytes; cap it so browsing a user can
/// never exhaust memory. Generous — real shares of hundreds of thousands of
/// files stay well under this.
pub const MAX_INFLATED_LEN: usize = 64 * 1024 * 1024;

/// Capacity hint cap: never preallocate more than this many elements from an
/// attacker-controlled count, even though the count claims more. The `Reader`
/// will still fault cleanly if the count was a lie and the bytes run out.
const MAX_PREALLOC: usize = 1024;

/// Upper bound on a UserInfo picture blob, so a hostile peer can't make us
/// allocate an arbitrary buffer from a forged length.
pub const MAX_PICTURE_LEN: usize = 8 * 1024 * 1024;

/// Peer code 4 — GetSharedFileList: ask a peer for their full share tree.
/// No body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GetSharedFileList;

impl GetSharedFileList {
    pub fn to_frame(&self) -> Vec<u8> {
        frame_u32(code::GET_SHARED_FILE_LIST, &[])
    }
}

/// One shared file: its name within the directory, size, extension, and the
/// optional audio attributes (bitrate, duration, …) the peer advertised.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SharedFile {
    pub name: String,
    pub size: u64,
    pub extension: String,
    /// `(attribute_type, value)` pairs; type 0 = bitrate, 1 = duration (s),
    /// 2 = vbr, 4 = sample rate, 5 = bit depth. Kept raw so the caller decides
    /// what to surface.
    pub attributes: Vec<(u32, u32)>,
}

/// One shared directory: its virtual path (e.g. `Music\\Album`) and files.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SharedDirectory {
    pub path: String,
    pub files: Vec<SharedFile>,
}

/// Peer code 5 — SharedFileListResponse: a peer's browsable share tree. The
/// `private` directories are buddy-only shares some clients append; older
/// clients omit the trailing section entirely.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SharedFileListResponse {
    pub directories: Vec<SharedDirectory>,
    pub private_directories: Vec<SharedDirectory>,
}

impl SharedFileListResponse {
    /// Decodes the (already zlib-decompressed) directory tree.
    ///
    /// Layout matches Nicotine+'s `SharedFileListResponse._parse_network_message`:
    /// the public directories are followed by a u32 of unknown purpose (official
    /// clients always send 0), and then — only if more bytes remain — the
    /// private-directory list. Both trailing fields are read conditionally, so
    /// older peers that omit them decode cleanly too.
    fn decode_inflated(raw: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(raw);
        let directories = read_directories(&mut r)?;
        if !r.is_empty() {
            let _unknown = r.u32()?;
        }
        let private_directories = if r.is_empty() {
            Vec::new()
        } else {
            read_directories(&mut r)?
        };
        Ok(SharedFileListResponse { directories, private_directories })
    }

    /// Encodes to the full wire frame: `[len][code 5][zlib(tree)]`. Mirrors
    /// [`PeerMessage::decode`] and Nicotine+'s `make_network_message`: public
    /// directories, then the unknown=0 field every official client sends, then
    /// the private-directory list only when there is one.
    pub fn to_frame(&self) -> Vec<u8> {
        let mut raw = Vec::new();
        write_directories(&mut raw, &self.directories);
        put_u32(&mut raw, 0); // unknown field; official clients always send 0
        if !self.private_directories.is_empty() {
            write_directories(&mut raw, &self.private_directories);
        }
        frame_u32(code::SHARED_FILE_LIST, &zlib_compress(&raw))
    }
}

/// One file entry as it appears in every share/search/folder listing
/// (Nicotine+'s `FileListMessage.pack_file_info`).
fn read_file(r: &mut Reader) -> Result<SharedFile, DecodeError> {
    let _code = r.u8()?; // always 1, unused
    let name = r.string()?;
    // Nicotine+'s `unpack_file_size` workaround for the Soulseek NS >2 GiB bug,
    // not a plain u64.
    let size = r.file_size()?;
    let extension = r.string()?;
    let attr_count = r.u32()? as usize;
    let mut attributes = Vec::with_capacity(attr_count.min(MAX_PREALLOC));
    for _ in 0..attr_count {
        let attr_type = r.u32()?;
        let value = r.u32()?;
        attributes.push((attr_type, value));
    }
    Ok(SharedFile { name, size, extension, attributes })
}

fn write_file(buf: &mut Vec<u8>, file: &SharedFile) {
    put_u8(buf, 1);
    put_string(buf, &file.name);
    put_u64(buf, file.size);
    put_string(buf, &file.extension);
    put_u32(buf, file.attributes.len() as u32);
    for &(attr_type, value) in &file.attributes {
        put_u32(buf, attr_type);
        put_u32(buf, value);
    }
}

/// A count-prefixed file list (`[u32 nfiles][file…]`).
fn read_file_list(r: &mut Reader) -> Result<Vec<SharedFile>, DecodeError> {
    let count = r.u32()? as usize;
    let mut files = Vec::with_capacity(count.min(MAX_PREALLOC));
    for _ in 0..count {
        files.push(read_file(r)?);
    }
    Ok(files)
}

fn write_file_list(buf: &mut Vec<u8>, files: &[SharedFile]) {
    put_u32(buf, files.len() as u32);
    for file in files {
        write_file(buf, file);
    }
}

fn read_directories(r: &mut Reader) -> Result<Vec<SharedDirectory>, DecodeError> {
    let count = r.u32()? as usize;
    let mut dirs = Vec::with_capacity(count.min(MAX_PREALLOC));
    for _ in 0..count {
        let path = r.string()?;
        let files = read_file_list(r)?;
        dirs.push(SharedDirectory { path, files });
    }
    Ok(dirs)
}

fn write_directories(buf: &mut Vec<u8>, dirs: &[SharedDirectory]) {
    put_u32(buf, dirs.len() as u32);
    for dir in dirs {
        put_string(buf, &dir.path);
        write_file_list(buf, &dir.files);
    }
}

fn zlib_compress(data: &[u8]) -> Vec<u8> {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;

    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).expect("writing to an in-memory zlib encoder");
    encoder.finish().expect("finishing an in-memory zlib encoder")
}

/// Inflates a zlib stream, refusing to expand past [`MAX_INFLATED_LEN`] so a
/// decompression bomb can't exhaust memory.
fn zlib_decompress(data: &[u8]) -> Result<Vec<u8>, DecodeError> {
    use flate2::read::ZlibDecoder;

    let mut out = Vec::new();
    // `take` caps how many bytes we'll ever read out of the decoder.
    let mut limited = ZlibDecoder::new(data).take(MAX_INFLATED_LEN as u64 + 1);
    limited
        .read_to_end(&mut out)
        .map_err(|e| DecodeError::InvalidValue(format!("zlib decompression failed: {e}")))?;
    if out.len() > MAX_INFLATED_LEN {
        return Err(DecodeError::InvalidValue(format!(
            "shared file list inflates past {MAX_INFLATED_LEN} bytes"
        )));
    }
    Ok(out)
}

/// Peer code 9 — FileSearchResponse: files of ours that matched a peer's search
/// (or, when we are the searcher, a peer's matches for us). zlib-compressed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileSearchResponse {
    pub username: String,
    pub token: u32,
    pub files: Vec<SharedFile>,
    pub free_slots: bool,
    pub upload_speed: u32,
    pub in_queue: u32,
    pub private_files: Vec<SharedFile>,
}

impl FileSearchResponse {
    fn decode_inflated(raw: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(raw);
        let username = r.string()?;
        let token = r.u32()?;
        let files = read_file_list(&mut r)?;
        let free_slots = r.bool()?;
        let upload_speed = r.u32()?;
        let in_queue = r.u32()?;
        // unknown=0, then optional private list — both conditional, as in
        // Nicotine+'s parser.
        if !r.is_empty() {
            let _unknown = r.u32()?;
        }
        let private_files = if r.is_empty() { Vec::new() } else { read_file_list(&mut r)? };
        Ok(FileSearchResponse {
            username,
            token,
            files,
            free_slots,
            upload_speed,
            in_queue,
            private_files,
        })
    }

    pub fn to_frame(&self) -> Vec<u8> {
        let mut raw = Vec::new();
        put_string(&mut raw, &self.username);
        put_u32(&mut raw, self.token);
        write_file_list(&mut raw, &self.files);
        put_bool(&mut raw, self.free_slots);
        put_u32(&mut raw, self.upload_speed);
        put_u32(&mut raw, self.in_queue);
        put_u32(&mut raw, 0); // unknown; official clients always send 0
        if !self.private_files.is_empty() {
            write_file_list(&mut raw, &self.private_files);
        }
        frame_u32(code::FILE_SEARCH_RESPONSE, &zlib_compress(&raw))
    }
}

/// Peer code 36 — FolderContentsRequest: "send me the files in this folder".
/// Not compressed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FolderContentsRequest {
    pub token: u32,
    pub directory: String,
}

impl FolderContentsRequest {
    pub fn to_frame(&self) -> Vec<u8> {
        let mut body = Vec::new();
        put_u32(&mut body, self.token);
        put_string(&mut body, &self.directory);
        frame_u32(code::FOLDER_CONTENTS_REQUEST, &body)
    }

    fn decode(r: &mut Reader) -> Result<Self, DecodeError> {
        Ok(FolderContentsRequest { token: r.u32()?, directory: r.string()? })
    }
}

/// Peer code 37 — FolderContentsResponse: the files in the requested folder(s).
/// zlib-compressed. Nicotine+ emits exactly the one requested directory, but the
/// wire format is a directory *list*, so we model and decode all of them rather
/// than dropping any past the first.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FolderContentsResponse {
    pub token: u32,
    /// The folder that was requested (the outer field; usually the same path as
    /// the single directory in `folders`).
    pub directory: String,
    pub folders: Vec<SharedDirectory>,
}

impl FolderContentsResponse {
    fn decode_inflated(raw: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(raw);
        let token = r.u32()?;
        let directory = r.string()?;
        let folders = read_directories(&mut r)?;
        // Nicotine+ appends a trailing u32 (always 0); ignore whatever remains.
        Ok(FolderContentsResponse { token, directory, folders })
    }

    pub fn to_frame(&self) -> Vec<u8> {
        let mut raw = Vec::new();
        put_u32(&mut raw, self.token);
        put_string(&mut raw, &self.directory);
        write_directories(&mut raw, &self.folders);
        put_u32(&mut raw, 0); // trailing field Nicotine+ appends
        frame_u32(code::FOLDER_CONTENTS_RESPONSE, &zlib_compress(&raw))
    }
}

/// Peer code 15 — UserInfoRequest: "tell me about yourself". No body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserInfoRequest;

impl UserInfoRequest {
    pub fn to_frame(&self) -> Vec<u8> {
        frame_u32(code::USER_INFO_REQUEST, &[])
    }
}

/// Peer code 16 — UserInfoResponse. Not compressed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UserInfoResponse {
    pub description: String,
    pub picture: Option<Vec<u8>>,
    pub total_uploads: u32,
    pub queue_size: u32,
    pub slots_available: bool,
    pub upload_allowed: u32,
}

impl UserInfoResponse {
    pub fn to_frame(&self) -> Vec<u8> {
        let mut body = Vec::new();
        put_string(&mut body, &self.description);
        match &self.picture {
            Some(pic) => {
                put_bool(&mut body, true);
                put_u32(&mut body, pic.len() as u32);
                body.extend_from_slice(pic);
            }
            None => put_bool(&mut body, false),
        }
        put_u32(&mut body, self.total_uploads);
        put_u32(&mut body, self.queue_size);
        put_bool(&mut body, self.slots_available);
        put_u32(&mut body, self.upload_allowed);
        frame_u32(code::USER_INFO_RESPONSE, &body)
    }

    fn decode(r: &mut Reader) -> Result<Self, DecodeError> {
        let description = r.string()?;
        let picture = if r.bool()? {
            let len = r.u32()? as usize;
            if len > MAX_PICTURE_LEN {
                return Err(DecodeError::InvalidValue(format!(
                    "user-info picture of {len} bytes exceeds {MAX_PICTURE_LEN}"
                )));
            }
            Some(r.bytes(len)?)
        } else {
            None
        };
        let total_uploads = r.u32()?;
        let queue_size = r.u32()?;
        let slots_available = r.bool()?;
        // Some clients omit the trailing upload_allowed field.
        let upload_allowed = if r.remaining() >= 4 { r.u32()? } else { 0 };
        Ok(UserInfoResponse {
            description,
            picture,
            total_uploads,
            queue_size,
            slots_available,
            upload_allowed,
        })
    }
}

/// A decoded peer message. Unknown codes are preserved rather than dropped, so
/// callers can log or count them — matching [`crate::server::ServerMessage`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerMessage {
    SharedFileListRequest,
    SharedFileList(SharedFileListResponse),
    FileSearchResponse(FileSearchResponse),
    FolderContentsRequest(FolderContentsRequest),
    FolderContents(FolderContentsResponse),
    UserInfoRequest,
    UserInfoResponse(UserInfoResponse),
    Unknown { code: u32, body: Vec<u8> },
}

impl PeerMessage {
    /// Decodes a frame payload (message code + contents) as produced by
    /// [`crate::frame::split_frame`].
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(payload);
        let code = r.u32()?;
        // For zlib-compressed messages, the rest of the payload after the 4-byte
        // code is a single zlib stream.
        let compressed = &payload[4.min(payload.len())..];
        match code {
            code::GET_SHARED_FILE_LIST => Ok(PeerMessage::SharedFileListRequest),
            code::SHARED_FILE_LIST => {
                let raw = zlib_decompress(compressed)?;
                Ok(PeerMessage::SharedFileList(SharedFileListResponse::decode_inflated(&raw)?))
            }
            code::FILE_SEARCH_RESPONSE => {
                let raw = zlib_decompress(compressed)?;
                Ok(PeerMessage::FileSearchResponse(FileSearchResponse::decode_inflated(&raw)?))
            }
            code::FOLDER_CONTENTS_REQUEST => {
                Ok(PeerMessage::FolderContentsRequest(FolderContentsRequest::decode(&mut r)?))
            }
            code::FOLDER_CONTENTS_RESPONSE => {
                let raw = zlib_decompress(compressed)?;
                Ok(PeerMessage::FolderContents(FolderContentsResponse::decode_inflated(&raw)?))
            }
            code::USER_INFO_REQUEST => Ok(PeerMessage::UserInfoRequest),
            code::USER_INFO_RESPONSE => {
                Ok(PeerMessage::UserInfoResponse(UserInfoResponse::decode(&mut r)?))
            }
            _ => {
                let mut body = Vec::with_capacity(r.remaining());
                while !r.is_empty() {
                    body.push(r.u8()?);
                }
                Ok(PeerMessage::Unknown { code, body })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::split_frame;

    #[test]
    fn get_shared_file_list_frame_is_byte_exact() {
        // length = code(4) + body(0) = 4, code 4, no body.
        assert_eq!(
            GetSharedFileList.to_frame(),
            [0x04, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00]
        );
    }

    fn sample_tree() -> SharedFileListResponse {
        SharedFileListResponse {
            directories: vec![
                SharedDirectory {
                    path: "Music\\Album".into(),
                    files: vec![
                        SharedFile {
                            name: "01 - Intro.mp3".into(),
                            size: 5_242_880,
                            extension: "mp3".into(),
                            attributes: vec![(0, 320), (1, 184)], // 320 kbps, 184 s
                        },
                        SharedFile {
                            name: "02 - Outro.flac".into(),
                            size: 31_457_280,
                            extension: "flac".into(),
                            attributes: vec![],
                        },
                    ],
                },
                SharedDirectory { path: "Music\\Empty".into(), files: vec![] },
            ],
            private_directories: vec![],
        }
    }

    #[test]
    fn shared_file_list_round_trips_through_a_frame() {
        let original = sample_tree();
        let frame = original.to_frame();

        // Split it back off as a receive loop would, then decode.
        let (payload, rest) = split_frame(&frame).unwrap().unwrap();
        assert!(rest.is_empty());
        assert_eq!(
            PeerMessage::decode(payload).unwrap(),
            PeerMessage::SharedFileList(original)
        );
    }

    #[test]
    fn private_directories_round_trip_when_present() {
        let original = SharedFileListResponse {
            directories: vec![SharedDirectory { path: "Public".into(), files: vec![] }],
            private_directories: vec![SharedDirectory {
                path: "Buddies Only".into(),
                files: vec![SharedFile {
                    name: "secret.mp3".into(),
                    size: 1,
                    extension: "mp3".into(),
                    attributes: vec![],
                }],
            }],
        };
        let frame = original.to_frame();
        let (payload, _) = split_frame(&frame).unwrap().unwrap();
        assert_eq!(
            PeerMessage::decode(payload).unwrap(),
            PeerMessage::SharedFileList(original)
        );
    }

    #[test]
    fn missing_private_section_decodes_as_empty() {
        // Hand-build an inflated tree with the public section only (no trailing
        // private-directory count), as an older client would send.
        let mut raw = Vec::new();
        write_directories(&mut raw, &[SharedDirectory { path: "A".into(), files: vec![] }]);
        let frame = frame_u32(code::SHARED_FILE_LIST, &zlib_compress(&raw));

        let (payload, _) = split_frame(&frame).unwrap().unwrap();
        let PeerMessage::SharedFileList(resp) = PeerMessage::decode(payload).unwrap() else {
            panic!("expected a shared file list");
        };
        assert_eq!(resp.directories.len(), 1);
        assert!(resp.private_directories.is_empty());
    }

    #[test]
    fn nicotine_layout_with_unknown_field_and_private_dirs_is_decoded_fully() {
        // Hand-build the inflated body in Nicotine+'s exact order:
        //   [public dirs][u32 unknown=0][private dirs]
        // The bug this guards against: reading the `unknown` u32 as the
        // private-directory count and dropping the private shares entirely.
        let mut raw = Vec::new();
        // public: one directory, no files
        put_u32(&mut raw, 1);
        put_string(&mut raw, "Public");
        put_u32(&mut raw, 0);
        // the unknown field official clients always send
        put_u32(&mut raw, 0);
        // private: one directory, one file
        put_u32(&mut raw, 1);
        put_string(&mut raw, "Buddies Only");
        put_u32(&mut raw, 1);
        put_u8(&mut raw, 1);
        put_string(&mut raw, "secret.mp3");
        put_u64(&mut raw, 4096);
        put_string(&mut raw, ""); // empty ext, as official clients send
        put_u32(&mut raw, 0); // no attributes

        let frame = frame_u32(code::SHARED_FILE_LIST, &zlib_compress(&raw));
        let (payload, _) = split_frame(&frame).unwrap().unwrap();
        let PeerMessage::SharedFileList(resp) = PeerMessage::decode(payload).unwrap() else {
            panic!("expected a shared file list");
        };

        assert_eq!(resp.directories.len(), 1);
        assert_eq!(resp.directories[0].path, "Public");
        assert_eq!(resp.private_directories.len(), 1, "private shares must not be dropped");
        assert_eq!(resp.private_directories[0].path, "Buddies Only");
        assert_eq!(resp.private_directories[0].files[0].name, "secret.mp3");
        assert_eq!(resp.private_directories[0].files[0].size, 4096);
    }

    #[test]
    fn oversized_file_size_uses_soulseek_ns_workaround() {
        // A buggy Soulseek NS peer sends a >2 GiB file with the low word holding
        // the real size and the high word set to 0xFFFFFFFF. Nicotine+'s
        // `unpack_file_size` (slskmessages.py:3208) detects offset+7 == 255 and
        // keeps only the low 32 bits. Without the workaround this size would
        // decode as ~16 EiB; we pin the corrected value here.
        let mut raw = Vec::new();
        put_u32(&mut raw, 1);
        put_string(&mut raw, "Music");
        put_u32(&mut raw, 1);
        put_u8(&mut raw, 1);
        put_string(&mut raw, "huge.flac");
        // size: low word = 100 MiB, high word = 0xFFFFFFFF garbage.
        put_u32(&mut raw, 100 * 1024 * 1024);
        put_u32(&mut raw, 0xFFFF_FFFF);
        put_string(&mut raw, ""); // empty ext
        put_u32(&mut raw, 0); // no attributes

        let frame = frame_u32(code::SHARED_FILE_LIST, &zlib_compress(&raw));
        let (payload, _) = split_frame(&frame).unwrap().unwrap();
        let PeerMessage::SharedFileList(resp) = PeerMessage::decode(payload).unwrap() else {
            panic!("expected a shared file list");
        };
        assert_eq!(resp.directories[0].files[0].size, 100 * 1024 * 1024);
    }

    #[test]
    fn empty_share_list_round_trips() {
        // Nicotine+ sends `pack_uint32(0)` for the folder count when a peer has
        // no shares (or its share DB fails to read); decoding yields an empty
        // tree, not an error. Pin the fully-empty round trip.
        let original = SharedFileListResponse::default();
        let frame = original.to_frame();
        let (payload, _) = split_frame(&frame).unwrap().unwrap();
        assert_eq!(
            PeerMessage::decode(payload).unwrap(),
            PeerMessage::SharedFileList(original)
        );
    }

    fn sample_file(name: &str, size: u64) -> SharedFile {
        SharedFile {
            name: name.into(),
            size,
            extension: String::new(),
            attributes: vec![(0, 320), (1, 184)], // bitrate, length
        }
    }

    fn decode_frame(frame: &[u8]) -> PeerMessage {
        let (payload, rest) = split_frame(frame).unwrap().unwrap();
        assert!(rest.is_empty());
        PeerMessage::decode(payload).unwrap()
    }

    #[test]
    fn file_search_response_round_trips_with_private_files() {
        let original = FileSearchResponse {
            username: "us".into(),
            token: 0xCAFE_F00D,
            files: vec![sample_file("Music\\a.mp3", 5_000_000), sample_file("Music\\b.flac", 30_000_000)],
            free_slots: true,
            upload_speed: 123_456,
            in_queue: 2,
            private_files: vec![sample_file("Buddies\\c.mp3", 1)],
        };
        assert_eq!(decode_frame(&original.to_frame()), PeerMessage::FileSearchResponse(original));
    }

    #[test]
    fn file_search_response_round_trips_without_private_files() {
        let original = FileSearchResponse {
            username: "us".into(),
            token: 7,
            files: vec![sample_file("x.mp3", 10)],
            free_slots: false,
            upload_speed: 0,
            in_queue: 0,
            private_files: vec![],
        };
        assert_eq!(decode_frame(&original.to_frame()), PeerMessage::FileSearchResponse(original));
    }

    #[test]
    fn folder_contents_request_and_response_round_trip() {
        let request = FolderContentsRequest { token: 42, directory: "Music\\Album".into() };
        assert_eq!(decode_frame(&request.to_frame()), PeerMessage::FolderContentsRequest(request));

        let response = FolderContentsResponse {
            token: 42,
            directory: "Music\\Album".into(),
            folders: vec![SharedDirectory {
                path: "Music\\Album".into(),
                files: vec![sample_file("song.mp3", 4096)],
            }],
        };
        assert_eq!(decode_frame(&response.to_frame()), PeerMessage::FolderContents(response));

        // Multiple directories round-trip too (not dropped past the first).
        let multi = FolderContentsResponse {
            token: 7,
            directory: "Root".into(),
            folders: vec![
                SharedDirectory { path: "Root\\A".into(), files: vec![sample_file("a.mp3", 1)] },
                SharedDirectory { path: "Root\\B".into(), files: vec![sample_file("b.mp3", 2)] },
            ],
        };
        assert_eq!(decode_frame(&multi.to_frame()), PeerMessage::FolderContents(multi));
    }

    #[test]
    fn user_info_request_and_response_round_trip() {
        assert_eq!(decode_frame(&UserInfoRequest.to_frame()), PeerMessage::UserInfoRequest);

        let with_pic = UserInfoResponse {
            description: "hi there".into(),
            picture: Some(vec![1, 2, 3, 4]),
            total_uploads: 9,
            queue_size: 3,
            slots_available: true,
            upload_allowed: 1,
        };
        assert_eq!(decode_frame(&with_pic.to_frame()), PeerMessage::UserInfoResponse(with_pic));

        let no_pic = UserInfoResponse {
            description: "no pic".into(),
            picture: None,
            total_uploads: 0,
            queue_size: 0,
            slots_available: false,
            upload_allowed: 0,
        };
        assert_eq!(decode_frame(&no_pic.to_frame()), PeerMessage::UserInfoResponse(no_pic));
    }

    #[test]
    fn shared_file_list_request_decodes() {
        assert_eq!(decode_frame(&GetSharedFileList.to_frame()), PeerMessage::SharedFileListRequest);
    }

    #[test]
    fn unknown_peer_codes_are_preserved() {
        let frame = frame_u32(42, &[1, 2, 3]);
        let (payload, _) = split_frame(&frame).unwrap().unwrap();
        assert_eq!(
            PeerMessage::decode(payload).unwrap(),
            PeerMessage::Unknown { code: 42, body: vec![1, 2, 3] }
        );
    }

    #[test]
    fn corrupt_zlib_stream_is_an_error_not_a_panic() {
        // Code 5 but the body is not a valid zlib stream.
        let frame = frame_u32(code::SHARED_FILE_LIST, &[0xFF, 0xFF, 0xFF, 0xFF]);
        let (payload, _) = split_frame(&frame).unwrap().unwrap();
        assert!(matches!(
            PeerMessage::decode(payload),
            Err(DecodeError::InvalidValue(_))
        ));
    }

    #[test]
    fn truncated_inflated_tree_is_an_error_not_a_panic() {
        // A valid zlib stream whose contents claim more directories than the
        // bytes provide must fault cleanly, not panic or hang.
        let mut raw = Vec::new();
        put_u32(&mut raw, 5); // claims 5 directories
        // ...but provide none.
        let frame = frame_u32(code::SHARED_FILE_LIST, &zlib_compress(&raw));
        let (payload, _) = split_frame(&frame).unwrap().unwrap();
        assert!(matches!(
            PeerMessage::decode(payload),
            Err(DecodeError::UnexpectedEof { .. })
        ));
    }

    #[test]
    fn decompression_bomb_is_rejected() {
        // 100 MB of zeroes compresses tiny but must be refused on inflate.
        let bomb = zlib_compress(&vec![0u8; (MAX_INFLATED_LEN) + 1024]);
        let frame = frame_u32(code::SHARED_FILE_LIST, &bomb);
        let (payload, _) = split_frame(&frame).unwrap().unwrap();
        assert!(matches!(
            PeerMessage::decode(payload),
            Err(DecodeError::InvalidValue(msg)) if msg.contains("inflates past")
        ));
    }
}
