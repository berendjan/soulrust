#!/usr/bin/env python3
"""Generate byte-vector fixtures from Nicotine+'s own message packers.

This makes Nicotine+ the *executed* oracle for our codec: rather than trusting a
human (or an LLM) to read the reference correctly, we run Nicotine+'s real
`make_network_message()` / pack helpers and emit the exact bytes. The companion
Rust test (crates/soulseek-proto/tests/nicotine_vectors.rs) then asserts our
encoder produces the same bytes and our decoder accepts Nicotine+'s output.

Usage:
    NICOTINE_DIR=../nicotine-plus python3 scripts/gen-nicotine-vectors.py

It writes crates/soulseek-proto/tests/fixtures/nicotine_vectors.rs (committed).
Re-run it whenever the Nicotine+ checkout is updated.
"""

import os
import socket
import struct
import sys
import zlib

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
NICOTINE_DIR = os.environ.get(
    "NICOTINE_DIR", os.path.join(os.path.dirname(REPO_ROOT), "nicotine-plus")
)
OUT_PATH = os.path.join(
    REPO_ROOT, "crates", "soulseek-proto", "tests", "fixtures", "nicotine_vectors.rs"
)

sys.path.insert(0, NICOTINE_DIR)

try:
    from pynicotine.slskmessages import (
        ConnectToPeer,
        ExcludedSearchPhrases,
        FileListMessage,
        FileOffset,
        FileSearch,
        FileSearchResponse,
        FileTransferInit,
        FolderContentsRequest,
        FolderContentsResponse,
        GetPeerAddress,
        Login,
        PeerInit,
        PierceFireWall,
        PlaceInQueueRequest,
        PlaceInQueueResponse,
        QueueUpload,
        SetWaitPort,
        SharedFileListRequest,
        SlskMessage,
        TransferRequest,
        TransferResponse,
        UploadDenied,
        UploadFailed,
        UserInfoRequest,
        UserInfoResponse,
    )
except ImportError as err:
    sys.exit(f"cannot import pynicotine from {NICOTINE_DIR}: {err}\n"
             f"set NICOTINE_DIR to your nicotine-plus checkout")


def rust_bytes(data: bytes) -> str:
    return "&[" + ", ".join(f"0x{b:02x}" for b in data) + "]"


def frame_u32(code: int, body: bytes) -> bytes:
    """The full wire frame for a u32-code message: [len][code][body]."""
    return struct.pack("<I", len(body) + 4) + struct.pack("<I", code) + body


def shared_file_list_frame() -> bytes:
    """Build a SharedFileListResponse (peer code 5) using Nicotine+'s own pack
    helpers, in Nicotine+'s exact order: public dirs, the unknown=0 field, then
    a private-dirs list. Compressed and framed exactly as on the wire."""
    def folder(name, files):
        out = SlskMessage.pack_string(name)
        out += SlskMessage.pack_uint32(len(files))
        for fileinfo in files:
            out += FileListMessage.pack_file_info(fileinfo)
        return out

    inflated = bytearray()
    # public: one folder, one file (fileinfo = (path, size, quality, duration))
    inflated += SlskMessage.pack_uint32(1)
    inflated += folder("Music\\Album", [("Music\\Album\\song.mp3", 5242880, None, None)])
    # the unknown field official clients always send
    inflated += SlskMessage.pack_uint32(0)
    # private: one folder, one file
    inflated += SlskMessage.pack_uint32(1)
    inflated += folder("Buddies", [("Buddies\\secret.flac", 999, None, None)])

    return frame_u32(5, zlib.compress(bytes(inflated)))


def folder_contents_frame() -> bytes:
    """Build a FolderContentsResponse (peer code 37) using Nicotine+'s own
    make_network_message: token, requested dir, then one folder (ndir=1, dir,
    its file list) — and NO trailing field. `shares` is the precomputed
    per-folder bytes (file count + packed file infos), exactly as Nicotine+
    stores a scanned folder stream."""
    file_list = SlskMessage.pack_uint32(1)  # one file in the folder
    # fileinfo = (basename, size, h_quality, h_duration); None attrs => no attrs.
    file_list += FileListMessage.pack_file_info(("song.mp3", 5242880, None, None))
    body = FolderContentsResponse(
        directory="Music\\Album", token=1234, shares=bytes(file_list)
    ).make_network_message()  # already zlib-compressed
    return frame_u32(37, body)


# ---------------------------------------------------------------------------
# Decode oracle for server->client / broadcast messages.
#
# Nicotine+'s `make_network_message` only packs the direction it *sends* (client
# requests + peer responses), so the server->client responses and relayed
# broadcasts we only ever decode have no packer to borrow. Instead we lay the
# bytes out with the wire primitives, then run Nicotine+'s OWN
# `parse_network_message` over them and assert it recovers the expected fields —
# making the reference the executed oracle for our *decoder*. The Rust test then
# decodes the identical bytes and asserts the same fields.

def s_str(text: str) -> bytes:
    raw = text.encode("utf-8")
    return struct.pack("<I", len(raw)) + raw


def s_u32(value: int) -> bytes:
    return struct.pack("<I", value)


def s_u16(value: int) -> bytes:
    return struct.pack("<H", value)


def s_bool(value: bool) -> bytes:
    return struct.pack("<B", 1 if value else 0)


def s_ip(dotted: str) -> bytes:
    # Nicotine+'s unpack_ip reverses the 4 bytes before inet_ntoa, so the wire
    # carries the octets reversed (little-endian u32 of the address).
    return socket.inet_aton(dotted)[::-1]


def parsed(cls, data: bytes):
    """Run Nicotine+'s parser over `data` and return the message object."""
    message = cls(msg_content=memoryview(bytes(data)))
    message.parse_network_message()
    return message


def decode_vectors():
    """Build + Nicotine+-validate each server->client / broadcast body."""
    out = []

    gpa = s_str("alice") + s_ip("198.51.100.7") + s_u32(2234) + s_u32(0) + s_u16(0)
    msg = parsed(GetPeerAddress, gpa)
    assert (msg.user, msg.ip_address, msg.port) == ("alice", "198.51.100.7", 2234), msg
    out.append((
        "GET_PEER_ADDRESS_RESPONSE_BODY",
        'GetPeerAddress response: user="alice", ip=198.51.100.7, port=2234, '
        "obfuscation_type=0, obfuscated_port=0 (u16)",
        gpa,
    ))

    ctp = (s_str("bob") + s_str("P") + s_ip("10.0.0.5") + s_u32(5000)
           + s_u32(0x01020304) + s_bool(False) + s_u32(0) + s_u32(0))
    msg = parsed(ConnectToPeer, ctp)
    assert (msg.user, msg.conn_type, msg.ip_address, msg.port, msg.token) == (
        "bob", "P", "10.0.0.5", 5000, 0x01020304), msg
    out.append((
        "CONNECT_TO_PEER_BODY",
        'ConnectToPeer (server->client): user="bob", conn_type="P", ip=10.0.0.5, '
        "port=5000, token=0x01020304, privileged=False, obfuscation_type=0, "
        "obfuscated_port=0 (u32)",
        ctp,
    ))

    ok = (s_bool(True) + s_str("Welcome to Soulseek!") + s_ip("203.0.113.9")
          + s_str("dbc93f24d8f3f109deed23c3e2f8b74c") + s_bool(True))
    msg = parsed(Login, ok)
    assert msg.success is True and msg.banner == "Welcome to Soulseek!", msg
    out.append((
        "LOGIN_RESPONSE_SUCCESS_BODY",
        'Login response (success): greeting="Welcome to Soulseek!", '
        "own_ip=203.0.113.9, password md5 checksum, is_supporter=True",
        ok,
    ))

    fail = s_bool(False) + s_str("INVALIDPASS") + s_str("invalid password")
    msg = parsed(Login, fail)
    assert msg.success is False and msg.rejection_reason == "INVALIDPASS", msg
    out.append((
        "LOGIN_RESPONSE_FAILURE_BODY",
        'Login response (failure): reason="INVALIDPASS", detail="invalid password" '
        "(the optional trailing rejection_detail)",
        fail,
    ))

    fs = s_str("searcher") + s_u32(0xABCD) + s_str("deep purple")
    msg = parsed(FileSearch, fs)
    assert (msg.search_username, msg.token, msg.searchterm) == (
        "searcher", 0xABCD, "deep purple"), msg
    out.append((
        "FILE_SEARCH_BROADCAST_BODY",
        'FileSearch as relayed by the server: search_username="searcher", '
        'token=0xABCD, searchterm="deep purple"',
        fs,
    ))

    esp = s_u32(2) + s_str("explicit") + s_str("banned phrase")
    msg = parsed(ExcludedSearchPhrases, esp)
    assert msg.phrases == ["explicit", "banned phrase"], msg
    out.append((
        "EXCLUDED_SEARCH_PHRASES_BODY",
        'ExcludedSearchPhrases: 2 phrases ["explicit", "banned phrase"]',
        esp,
    ))

    return out


# Each entry: (rust const name, comment, bytes). Bodies come straight from
# Nicotine+'s make_network_message(); the shared file list is a full frame.
VECTORS = [
    ("LOGIN_BODY",
     'Login(username="alice", passwd="s3cr3t", version=160, minorversion=1)',
     Login(username="alice", passwd="s3cr3t", version=160, minorversion=1).make_network_message()),
    ("SET_WAIT_PORT_BODY",
     "SetWaitPort(port=2234) — note: Nicotine+ sends PORT ONLY (no obfuscation fields)",
     SetWaitPort(port=2234).make_network_message()),
    ("GET_PEER_ADDRESS_BODY",
     'GetPeerAddress(user="alice")',
     GetPeerAddress(user="alice").make_network_message()),
    ("FILE_SEARCH_BODY",
     'FileSearch(token=0x12345678, text="test query")',
     FileSearch(token=0x12345678, text="test query").make_network_message()),
    ("PIERCE_FIREWALL_BODY",
     "PierceFireWall(token=4242)",
     PierceFireWall(token=4242).make_network_message()),
    ("PEER_INIT_BODY",
     'PeerInit(init_user="alice", conn_type="P")',
     PeerInit(init_user="alice", conn_type="P").make_network_message()),
    ("SHARED_FILE_LIST_REQUEST_BODY",
     "SharedFileListRequest() — empty body",
     SharedFileListRequest().make_network_message()),
    ("SHARED_FILE_LIST_FRAME",
     "SharedFileListResponse (peer code 5), full compressed frame; one public "
     "and one private folder, each with one file",
     shared_file_list_frame()),
    ("USER_INFO_RESPONSE_BODY",
     'UserInfoResponse(descr="soulrust user", pic=None, totalupl=42, queuesize=3, '
     "slotsavail=True, uploadallowed=1) — uncompressed body",
     UserInfoResponse(descr="soulrust user", pic=None, totalupl=42, queuesize=3,
                      slotsavail=True, uploadallowed=1).make_network_message()),
    ("FOLDER_CONTENTS_REQUEST_BODY",
     'FolderContentsRequest(directory="Music\\\\Album", token=1234) — uncompressed body',
     FolderContentsRequest(directory="Music\\Album", token=1234).make_network_message()),
    ("FOLDER_CONTENTS_RESPONSE_FRAME",
     "FolderContentsResponse (peer code 37), full compressed frame; token 1234, "
     'dir "Music\\\\Album", one folder with one file — no trailing field',
     folder_contents_frame()),
    ("USER_INFO_REQUEST_BODY",
     "UserInfoRequest() — empty body",
     UserInfoRequest().make_network_message()),
    ("FILE_SEARCH_RESPONSE_FRAME",
     "FileSearchResponse (peer code 9), full compressed frame; user 'peer', one file",
     frame_u32(9, FileSearchResponse(
         search_username="peer", token=0x2222,
         shares=[("Music\\hit.mp3", 4096, None, None)],
         freeulslots=True, ulspeed=5000, inqueue=0, private_shares=[]).make_network_message())),
    ("CONNECT_TO_PEER_REQUEST_BODY",
     'ConnectToPeer(token=0x01020304, user="alice", conn_type="P") — uncompressed body',
     ConnectToPeer(token=0x01020304, user="alice", conn_type="P").make_network_message()),
    # --- file transfers (peer codes 40/41/43/44/46/50/51 + raw F-connection) ---
    ("TRANSFER_REQUEST_UPLOAD_BODY",
     'TransferRequest(direction=UPLOAD=1, token=0xABCD, file="Music\\\\song.mp3", '
     "filesize=5242880) — direction, token, file, then the u64 size",
     TransferRequest(direction=1, token=0xABCD, file="Music\\song.mp3",
                     filesize=5242880).make_network_message()),
    ("TRANSFER_REQUEST_DOWNLOAD_BODY",
     'TransferRequest(direction=DOWNLOAD=0, token=7, file="x") — no trailing size',
     TransferRequest(direction=0, token=7, file="x").make_network_message()),
    ("TRANSFER_RESPONSE_ALLOWED_BODY",
     "TransferResponse(allowed=True, token=9, filesize=4096) — token, bool, u64 size",
     TransferResponse(allowed=True, token=9, filesize=4096).make_network_message()),
    ("TRANSFER_RESPONSE_REJECTED_BODY",
     'TransferResponse(allowed=False, token=9, reason="Queued") — token, bool, reason',
     TransferResponse(allowed=False, token=9, reason="Queued").make_network_message()),
    ("QUEUE_UPLOAD_BODY",
     'QueueUpload(file="Music\\\\song.mp3")',
     QueueUpload(file="Music\\song.mp3").make_network_message()),
    ("PLACE_IN_QUEUE_REQUEST_BODY",
     'PlaceInQueueRequest(file="a\\\\b.mp3")',
     PlaceInQueueRequest(file="a\\b.mp3").make_network_message()),
    ("PLACE_IN_QUEUE_RESPONSE_BODY",
     'PlaceInQueueResponse(filename="a\\\\b.mp3", place=3)',
     PlaceInQueueResponse(filename="a\\b.mp3", place=3).make_network_message()),
    ("UPLOAD_DENIED_BODY",
     'UploadDenied(file="a.mp3", reason="Not shared")',
     UploadDenied(file="a.mp3", reason="Not shared").make_network_message()),
    ("UPLOAD_FAILED_BODY",
     'UploadFailed(file="a.mp3")',
     UploadFailed(file="a.mp3").make_network_message()),
    ("FILE_TRANSFER_INIT_BYTES",
     "FileTransferInit(token=0xABCD) — raw F-connection bytes: a bare u32, no frame",
     FileTransferInit(token=0xABCD).make_network_message()),
    ("FILE_OFFSET_BYTES",
     "FileOffset(offset=1048576) — raw F-connection bytes: a bare u64, no frame",
     FileOffset(offset=1048576).make_network_message()),
]

# Server->client / broadcast bodies, laid out by hand and validated by running
# Nicotine+'s own parser over them (see decode_vectors).
VECTORS += decode_vectors()


def main():
    lines = [
        "// @generated by scripts/gen-nicotine-vectors.py — DO NOT EDIT.",
        "//",
        "// Byte vectors produced by executing Nicotine+'s own message packers, so",
        "// our codec is validated against the reference implementation's real output",
        "// rather than against a reading of it. Regenerate with:",
        "//   NICOTINE_DIR=../nicotine-plus python3 scripts/gen-nicotine-vectors.py",
        "",
    ]
    for name, comment, data in VECTORS:
        data = bytes(data)
        lines.append(f"/// {comment}")
        lines.append(f"pub const {name}: &[u8] = {rust_bytes(data)};")
        lines.append("")

    os.makedirs(os.path.dirname(OUT_PATH), exist_ok=True)
    with open(OUT_PATH, "w", encoding="utf-8") as handle:
        handle.write("\n".join(lines))

    print(f"wrote {OUT_PATH} ({len(VECTORS)} vectors)")
    for name, _comment, data in VECTORS:
        print(f"  {name}: {len(bytes(data))} bytes")


if __name__ == "__main__":
    main()
