//! The raw `F`-connection file streaming, used by the reactor.
//!
//! Once a transfer is negotiated on a `P` connection (`QueueUpload` →
//! `TransferRequest` → `TransferResponse`), the actual bytes move over a
//! separate `F` connection that, after a tiny header, is just raw file data:
//!
//! - **Uploader** sends [`FileTransferInit`] (a bare `u32` token), reads a
//!   [`FileOffset`] (a bare `u64`), seeks, and streams the file from there.
//! - **Downloader** reads the init token (matched by the caller), sends the
//!   offset to resume from, and reads the declared number of bytes to disk.
//!
//! Per the project rule, bulk bytes never touch the message bus: these
//! functions move data directly between the socket and a `tokio::fs::File`. The
//! socket side is generic over `AsyncRead + AsyncWrite` so the whole exchange is
//! testable over an in-memory duplex.

use std::io::{self, SeekFrom};

use soulseek_proto::transfer::{FileOffset, FileTransferInit};
use tokio::fs::File;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt};

/// Stream a file to a peer over an `F` connection (we are the uploader).
///
/// Sends `FileTransferInit(token)`, reads the peer's `FileOffset`, seeks there,
/// and writes the remaining `size - offset` bytes. Returns the number of bytes
/// sent. The 64 KiB chunking bounds memory regardless of file size.
pub async fn upload<S>(stream: &mut S, token: u32, mut file: File, size: u64) -> io::Result<u64>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    stream.write_all(&FileTransferInit { token }.to_bytes()).await?;

    let mut offset_buf = [0u8; FileOffset::LEN];
    stream.read_exact(&mut offset_buf).await?;
    let offset = FileOffset::decode(&offset_buf)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?
        .offset;
    if offset > size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "peer requested an offset past the end of the file",
        ));
    }

    file.seek(SeekFrom::Start(offset)).await?;
    let mut remaining = size - offset;
    let mut sent = 0u64;
    let mut buf = vec![0u8; 64 * 1024];
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        let read = file.read(&mut buf[..want]).await?;
        if read == 0 {
            // File is shorter than the size we advertised — stop cleanly rather
            // than spin; the peer sees a short transfer.
            break;
        }
        stream.write_all(&buf[..read]).await?;
        sent += read as u64;
        remaining -= read as u64;
    }
    stream.flush().await?;
    Ok(sent)
}

/// Receive a file from a peer over an `F` connection (we are the downloader).
///
/// The caller has already read and matched the peer's `FileTransferInit`. This
/// sends `FileOffset(offset)` (the resume point) and reads exactly
/// `expected_size - offset` bytes into `sink`, returning the number of bytes
/// written. A short close before that many bytes is an error.
pub async fn download<S>(
    stream: &mut S,
    offset: u64,
    expected_size: u64,
    mut sink: File,
) -> io::Result<u64>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    if offset > expected_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "resume offset past the declared file size",
        ));
    }
    stream.write_all(&FileOffset { offset }.to_bytes()).await?;

    let mut remaining = expected_size - offset;
    let mut received = 0u64;
    let mut buf = vec![0u8; 64 * 1024];
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        let read = stream.read(&mut buf[..want]).await?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "peer closed before sending the whole file",
            ));
        }
        sink.write_all(&buf[..read]).await?;
        received += read as u64;
        remaining -= read as u64;
    }
    sink.flush().await?;
    Ok(received)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tokio::io::AsyncReadExt;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }

    /// A unique temp path that removes itself on drop (the suite uses
    /// `std::env::temp_dir()` rather than a `tempfile` dependency).
    struct TempPath(PathBuf);
    impl TempPath {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("soulrust-xfer-{}-{n}-{tag}", std::process::id()));
            TempPath(path)
        }
    }
    impl Drop for TempPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn upload_sends_init_then_streams_from_the_requested_offset() {
        runtime().block_on(async {
            let contents = b"0123456789abcdef".repeat(8192); // 128 KiB, multi-chunk
            let src = TempPath::new("src");
            tokio::fs::write(&src.0, &contents).await.unwrap();
            let file = File::open(&src.0).await.unwrap();
            let (mut client, mut server) = tokio::io::duplex(64 * 1024);

            let size = contents.len() as u64;
            let up = tokio::spawn(async move { upload(&mut server, 0x2222, file, size).await });

            // Read the init token.
            let mut init = [0u8; FileTransferInit::LEN];
            client.read_exact(&mut init).await.unwrap();
            assert_eq!(FileTransferInit::decode(&init).unwrap().token, 0x2222);

            // Ask to resume from byte 16, then read the rest.
            client.write_all(&FileOffset { offset: 16 }.to_bytes()).await.unwrap();
            let mut got = Vec::new();
            client.read_to_end(&mut got).await.unwrap();

            let sent = up.await.unwrap().unwrap();
            assert_eq!(sent, size - 16);
            assert_eq!(got, &contents[16..]);
        });
    }

    #[test]
    fn download_sends_offset_then_writes_expected_bytes_to_disk() {
        runtime().block_on(async {
            let dest = TempPath::new("incomplete");
            let sink = File::create(&dest.0).await.unwrap();
            let (mut client, mut server) = tokio::io::duplex(64 * 1024);

            let payload = b"the quick brown fox".repeat(4096); // 76 KiB
            let size = payload.len() as u64;
            let dl = tokio::spawn(async move { download(&mut server, 0, size, sink).await });

            // We (the peer/uploader side of the duplex) read the offset, then send bytes.
            let mut offset_buf = [0u8; FileOffset::LEN];
            client.read_exact(&mut offset_buf).await.unwrap();
            assert_eq!(FileOffset::decode(&offset_buf).unwrap().offset, 0);
            client.write_all(&payload).await.unwrap();
            drop(client);

            let received = dl.await.unwrap().unwrap();
            assert_eq!(received, size);
            assert_eq!(tokio::fs::read(&dest.0).await.unwrap(), payload);
        });
    }

    #[test]
    fn download_errors_if_the_peer_closes_early() {
        runtime().block_on(async {
            let dest = TempPath::new("short");
            let sink = File::create(&dest.0).await.unwrap();
            let (mut client, mut server) = tokio::io::duplex(1024);

            let dl = tokio::spawn(async move { download(&mut server, 0, 1000, sink).await });
            let mut offset_buf = [0u8; FileOffset::LEN];
            client.read_exact(&mut offset_buf).await.unwrap();
            client.write_all(b"only a few bytes").await.unwrap();
            drop(client); // close before the 1000 promised bytes

            let result = dl.await.unwrap();
            assert!(result.is_err(), "short transfer must error");
        });
    }
}
