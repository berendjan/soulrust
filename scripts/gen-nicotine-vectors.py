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
        FileListMessage,
        FileSearch,
        FileSearchResponse,
        FolderContentsRequest,
        GetPeerAddress,
        Login,
        PeerInit,
        PierceFireWall,
        SetWaitPort,
        SharedFileListRequest,
        SlskMessage,
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
    ("FILE_SEARCH_RESPONSE_FRAME",
     "FileSearchResponse (peer code 9), full compressed frame; user 'peer', one file",
     frame_u32(9, FileSearchResponse(
         search_username="peer", token=0x2222,
         shares=[("Music\\hit.mp3", 4096, None, None)],
         freeulslots=True, ulspeed=5000, inqueue=0, private_shares=[]).make_network_message())),
    ("CONNECT_TO_PEER_REQUEST_BODY",
     'ConnectToPeer(token=0x01020304, user="alice", conn_type="P") — uncompressed body',
     ConnectToPeer(token=0x01020304, user="alice", conn_type="P").make_network_message()),
]


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
