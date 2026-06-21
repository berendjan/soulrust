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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use soulseek_proto::transfer::{FileOffset, FileTransferInit};
use tokio::fs::File;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt};

/// A token-bucket rate limiter shared across all transfers in one direction.
/// `rate` is bytes/second; `0` means unlimited (the hot path then does a single
/// relaxed atomic load and returns). The bucket allows up to one second of burst.
pub struct RateLimiter {
    rate: AtomicU64,
    state: Mutex<BucketState>,
}

struct BucketState {
    allowance: f64,
    last: Instant,
}

impl RateLimiter {
    fn new() -> Self {
        RateLimiter {
            rate: AtomicU64::new(0),
            state: Mutex::new(BucketState { allowance: 0.0, last: Instant::now() }),
        }
    }

    /// Set the limit in bytes/second (`0` = unlimited).
    pub fn set_rate(&self, bytes_per_sec: u64) {
        self.rate.store(bytes_per_sec, Ordering::Relaxed);
    }

    /// Wait until `n` bytes may pass under the current rate. Unlimited returns
    /// immediately; otherwise refills the bucket by elapsed time and sleeps off
    /// any deficit. Sleeping happens outside the lock.
    async fn acquire(&self, n: u64) {
        let rate = self.rate.load(Ordering::Relaxed);
        if rate == 0 {
            return;
        }
        let sleep = {
            let mut s = self.state.lock().unwrap();
            let now = Instant::now();
            let elapsed = now.duration_since(s.last).as_secs_f64();
            s.last = now;
            let cap = rate as f64; // one second of burst
            s.allowance = (s.allowance + elapsed * rate as f64).min(cap);
            s.allowance -= n as f64;
            if s.allowance < 0.0 {
                Some(Duration::from_secs_f64(-s.allowance / rate as f64))
            } else {
                None
            }
        };
        if let Some(delay) = sleep {
            tokio::time::sleep(delay).await;
        }
    }
}

/// Process-global (download, upload) limiters, shared by every transfer so the
/// caps are aggregate, not per-connection. Set from config at reactor start and
/// updated live on a config change.
static LIMITS: OnceLock<(RateLimiter, RateLimiter)> = OnceLock::new();

fn limits() -> &'static (RateLimiter, RateLimiter) {
    LIMITS.get_or_init(|| (RateLimiter::new(), RateLimiter::new()))
}

/// The shared download-direction limiter (bytes we receive).
pub fn download_limiter() -> &'static RateLimiter {
    &limits().0
}

/// The shared upload-direction limiter (bytes we send).
pub fn upload_limiter() -> &'static RateLimiter {
    &limits().1
}

/// Apply aggregate bandwidth caps (bytes/second; `0` = unlimited).
pub fn set_bandwidth_limits(download_bps: u64, upload_bps: u64) {
    download_limiter().set_rate(download_bps);
    upload_limiter().set_rate(upload_bps);
}

/// Copy exactly `len` bytes from `src` to `dst` in 64 KiB chunks, erroring if
/// `src` ends before `len` bytes have been read. The fixed buffer bounds memory
/// regardless of transfer size. `limiter` throttles throughput (unlimited by
/// default), gating each chunk against the shared per-direction token bucket.
async fn copy_exact<R, W>(
    src: &mut R,
    dst: &mut W,
    len: u64,
    limiter: &RateLimiter,
    progress: &mut (dyn FnMut(u64) + Send),
) -> io::Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut remaining = len;
    let mut buf = vec![0u8; 64 * 1024];
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        let read = src.read(&mut buf[..want]).await?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "source ended before the expected number of bytes",
            ));
        }
        limiter.acquire(read as u64).await;
        dst.write_all(&buf[..read]).await?;
        remaining -= read as u64;
        progress(len - remaining);
    }
    Ok(len)
}

/// Stream a file to a peer over an `F` connection (we are the uploader).
///
/// Sends `FileTransferInit(token)`, reads the peer's `FileOffset`, seeks there,
/// and writes the remaining `size - offset` bytes. Returns the number of bytes
/// sent. A file shorter than the advertised `size` errors rather than silently
/// short-transferring (the peer would otherwise wait for bytes that never come).
pub async fn upload<S>(
    stream: &mut S,
    token: u32,
    file: File,
    size: u64,
    progress: &mut (dyn FnMut(u64) + Send),
) -> io::Result<u64>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    stream.write_all(&FileTransferInit { token }.to_bytes()).await?;
    stream_from_offset(stream, file, size, progress).await
}

/// The second half of an upload, for when the `FileTransferInit` token has
/// already been exchanged: read the peer's `FileOffset`, seek there, and stream
/// the remaining `size - offset` bytes. This is the whole upload when *the
/// downloader* opened the file connection (it sent the token, so we must not
/// send it again) — the `TransferRequest{Download}` request path.
pub async fn stream_from_offset<S>(
    stream: &mut S,
    mut file: File,
    size: u64,
    progress: &mut (dyn FnMut(u64) + Send),
) -> io::Result<u64>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
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
    // Report absolute bytes sent (resume offset + this call's progress).
    let mut report = |done: u64| progress(offset + done);
    let sent = copy_exact(&mut file, stream, size - offset, upload_limiter(), &mut report).await?;
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
    progress: &mut (dyn FnMut(u64) + Send),
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
    // Flush whatever we received even if the transfer ends early: tokio's File
    // buffers writes, and a dropped buffer would lose the partial we need on disk
    // to resume later. Persist first, then surface any short-transfer error.
    // Report absolute bytes on disk (resume offset + this call's progress).
    let mut report = |done: u64| progress(offset + done);
    let copied =
        copy_exact(stream, &mut sink, expected_size - offset, download_limiter(), &mut report).await;
    sink.flush().await?;
    let received = copied?;
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

    #[test]
    fn rate_limiter_throttles_when_over_budget() {
        runtime().block_on(async {
            let limiter = RateLimiter::new();
            limiter.set_rate(1000); // 1000 B/s
            // From an empty bucket, 100 bytes is a 0.1s deficit at 1000 B/s.
            let start = Instant::now();
            limiter.acquire(100).await;
            assert!(
                start.elapsed() >= Duration::from_millis(80),
                "expected ~100ms throttle, got {:?}",
                start.elapsed()
            );
        });
    }

    #[test]
    fn rate_limiter_unlimited_returns_immediately() {
        runtime().block_on(async {
            let limiter = RateLimiter::new(); // rate 0 = unlimited
            let start = Instant::now();
            limiter.acquire(100_000_000).await;
            assert!(start.elapsed() < Duration::from_millis(50));
        });
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
            let up = tokio::spawn(async move { upload(&mut server, 0x2222, file, size, &mut |_| {}).await });

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
    fn stream_from_offset_streams_without_re_sending_the_token() {
        // The downloader-opened upload path (`TransferRequest{Download}`): the
        // token was already exchanged by the caller, so we send NO
        // `FileTransferInit` — we read the offset and stream straight away.
        runtime().block_on(async {
            let contents = b"0123456789abcdef".repeat(8192); // 128 KiB, multi-chunk
            let src = TempPath::new("src-offset");
            tokio::fs::write(&src.0, &contents).await.unwrap();
            let file = File::open(&src.0).await.unwrap();
            let (mut client, mut server) = tokio::io::duplex(64 * 1024);

            let size = contents.len() as u64;
            let up = tokio::spawn(async move { stream_from_offset(&mut server, file, size, &mut |_| {}).await });

            // No init token is read here (unlike `upload`): straight to the offset.
            client.write_all(&FileOffset { offset: 32 }.to_bytes()).await.unwrap();
            let mut got = Vec::new();
            client.read_to_end(&mut got).await.unwrap();

            let sent = up.await.unwrap().unwrap();
            assert_eq!(sent, size - 32);
            assert_eq!(got, &contents[32..]);
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
            let dl = tokio::spawn(async move { download(&mut server, 0, size, sink, &mut |_| {}).await });

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

            let dl = tokio::spawn(async move { download(&mut server, 0, 1000, sink, &mut |_| {}).await });
            let mut offset_buf = [0u8; FileOffset::LEN];
            client.read_exact(&mut offset_buf).await.unwrap();
            client.write_all(b"only a few bytes").await.unwrap();
            drop(client); // close before the 1000 promised bytes

            let result = dl.await.unwrap();
            assert!(result.is_err(), "short transfer must error");
        });
    }

    #[test]
    fn upload_of_a_fully_resumed_file_sends_zero_bytes() {
        runtime().block_on(async {
            let src = TempPath::new("done");
            tokio::fs::write(&src.0, b"abcdefghij").await.unwrap(); // 10 bytes
            let file = File::open(&src.0).await.unwrap();
            let (mut client, mut server) = tokio::io::duplex(1024);

            let up = tokio::spawn(async move { upload(&mut server, 1, file, 10, &mut |_| {}).await });
            let mut init = [0u8; FileTransferInit::LEN];
            client.read_exact(&mut init).await.unwrap();
            client.write_all(&FileOffset { offset: 10 }.to_bytes()).await.unwrap(); // resume at EOF

            let mut got = Vec::new();
            client.read_to_end(&mut got).await.unwrap();
            assert_eq!(up.await.unwrap().unwrap(), 0);
            assert!(got.is_empty());
        });
    }

    #[test]
    fn upload_rejects_an_offset_past_the_size() {
        runtime().block_on(async {
            let src = TempPath::new("small");
            tokio::fs::write(&src.0, b"abc").await.unwrap();
            let file = File::open(&src.0).await.unwrap();
            let (mut client, mut server) = tokio::io::duplex(1024);

            let up = tokio::spawn(async move { upload(&mut server, 1, file, 3, &mut |_| {}).await });
            let mut init = [0u8; FileTransferInit::LEN];
            client.read_exact(&mut init).await.unwrap();
            client.write_all(&FileOffset { offset: 9 }.to_bytes()).await.unwrap(); // past size

            assert!(up.await.unwrap().is_err());
        });
    }

    #[test]
    fn upload_errors_when_the_file_is_shorter_than_advertised() {
        runtime().block_on(async {
            let src = TempPath::new("truncated");
            tokio::fs::write(&src.0, b"only ten b").await.unwrap(); // 10 bytes on disk
            let file = File::open(&src.0).await.unwrap();
            let (mut client, mut server) = tokio::io::duplex(4096);

            // Advertise 1000 bytes but the file holds 10.
            let up = tokio::spawn(async move { upload(&mut server, 1, file, 1000, &mut |_| {}).await });
            let mut init = [0u8; FileTransferInit::LEN];
            client.read_exact(&mut init).await.unwrap();
            client.write_all(&FileOffset { offset: 0 }.to_bytes()).await.unwrap();
            let mut drained = Vec::new();
            let _ = client.read_to_end(&mut drained).await;

            assert!(up.await.unwrap().is_err(), "short file must surface an error, not Ok");
        });
    }

    #[test]
    fn download_rejects_an_offset_past_the_expected_size() {
        runtime().block_on(async {
            let dest = TempPath::new("bad-offset");
            let sink = File::create(&dest.0).await.unwrap();
            let (_client, mut server) = tokio::io::duplex(1024);
            let result = download(&mut server, 100, 50, sink, &mut |_| {}).await;
            assert!(result.is_err());
        });
    }
}
