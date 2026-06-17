//! Shared helpers for the Soulfind integration tests.
//!
//! These tests need Docker: they start the open-source Soulfind server
//! (`ghcr.io/soulfind-dev/soulfind`) via testcontainers and speak the real
//! protocol to it. They are tagged `docker` in BUILD.bazel and excluded from
//! default `bazel test //...` runs; use `bazel test --config=docker`.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use soulseek_proto::frame::split_frame;
use soulseek_proto::server::{LoginRequest, LoginResponse, ServerMessage, ServerRequest};
use testcontainers::core::{IntoContainerPort, Mount, WaitFor};
use testcontainers::runners::SyncRunner;
use testcontainers::{Container, GenericImage, ImageExt};

pub const SOULFIND_PORT: u16 = 2242;

/// Starts a Soulfind container and waits until it accepts logins.
pub fn start_soulfind() -> (Container<GenericImage>, u16) {
    let container = GenericImage::new("ghcr.io/soulfind-dev/soulfind", "latest")
        .with_exposed_port(SOULFIND_PORT.tcp())
        // Soulfind logs vary between versions; readiness is probed by
        // actually logging in below rather than by log matching.
        .with_wait_for(WaitFor::Nothing)
        .start()
        .expect("starting soulfind container (is Docker running?)");
    let port = container
        .get_host_port_ipv4(SOULFIND_PORT.tcp())
        .expect("mapped soulfind port");

    wait_until_login_accepted(port);
    (container, port)
}

/// Soulfind registers username+password on first login, so reusing a name
/// with a different password fails; every test gets a fresh identity.
pub fn unique_username(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}-{nanos}")
}

fn wait_until_login_accepted(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut last_err = String::new();
    while Instant::now() < deadline {
        match try_login(port, &unique_username("probe"), "probe-pass") {
            Ok(_) => return,
            Err(err) => last_err = err,
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    panic!("soulfind did not become ready within 60s; last error: {last_err}");
}

/// A logged-in (or attempted) connection with framing helpers.
pub struct ServerConnection {
    stream: TcpStream,
    pending: Vec<u8>,
}

impl ServerConnection {
    pub fn connect(port: u16) -> Result<Self, String> {
        let stream = TcpStream::connect(("127.0.0.1", port)).map_err(|e| e.to_string())?;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| e.to_string())?;
        Ok(ServerConnection { stream, pending: Vec::new() })
    }

    pub fn send_frame(&mut self, frame: &[u8]) -> Result<(), String> {
        self.stream.write_all(frame).map_err(|e| e.to_string())
    }

    /// Reads until one complete frame is available and decodes it.
    pub fn read_message(&mut self) -> Result<ServerMessage, String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut chunk = [0u8; 8192];
        loop {
            if let Some((payload, rest)) =
                split_frame(&self.pending).map_err(|e| e.to_string())?
            {
                let message = ServerMessage::decode(payload).map_err(|e| e.to_string())?;
                self.pending = rest.to_vec();
                return Ok(message);
            }
            if Instant::now() > deadline {
                return Err("timed out waiting for a server frame".into());
            }
            match self.stream.read(&mut chunk) {
                Ok(0) => return Err("server closed the connection".into()),
                Ok(n) => self.pending.extend_from_slice(&chunk[..n]),
                Err(e) => return Err(format!("read: {e}")),
            }
        }
    }

    /// Reads messages until the predicate matches (skipping unrelated server
    /// chatter like room lists and parent speed announcements).
    pub fn read_until<T>(
        &mut self,
        mut matcher: impl FnMut(ServerMessage) -> Option<T>,
    ) -> Result<T, String> {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if let Some(found) = matcher(self.read_message()?) {
                return Ok(found);
            }
        }
        Err("timed out waiting for the expected message".into())
    }

    pub fn login(&mut self, username: &str, password: &str) -> Result<LoginResponse, String> {
        let login = LoginRequest {
            username: username.into(),
            password: password.into(),
            major_version: 160,
            minor_version: 1,
        };
        self.send_frame(&login.to_frame())?;
        self.read_until(|message| match message {
            ServerMessage::Login(response) => Some(response),
            _ => None,
        })
    }
}

/// One full connect+login round trip; used by the readiness probe.
pub fn try_login(port: u16, username: &str, password: &str) -> Result<LoginResponse, String> {
    let mut connection = ServerConnection::connect(port)?;
    connection.login(username, password)
}

/// Picks a free TCP port by binding port 0 and releasing it. Slightly racy
/// by nature; good enough for tests.
pub fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().unwrap().port()
}

/// A running [slskd](https://github.com/slskd/slskd) peer, logged into the same
/// Soulfind server as the system under test, driven over its REST API.
///
/// The container runs on the **host network** so peer-to-peer dials resolve the
/// same way for everyone: slskd connects to Soulfind on `127.0.0.1`, advertises
/// a real listen port, and is reachable from the host-side soulrust process at
/// the address Soulfind hands out. This relies on Linux Docker networking and
/// is why the test is tagged `docker`/`local`/`requires-network`.
///
/// `shared_dir` must already be populated before `start` (slskd scans shares on
/// startup); `downloads_dir`/`incomplete_dir` are bind-mounted so the test can
/// read transferred bytes back from the host.
pub struct Slskd {
    // Dropping the container stops and removes it; keep it alive for the test.
    _container: Container<GenericImage>,
    base: String,
    pub username: String,
    pub listen_port: u16,
    pub downloads_dir: PathBuf,
}

impl Slskd {
    pub fn start(
        soulfind_port: u16,
        username: &str,
        password: &str,
        shared_dir: &Path,
        downloads_dir: &Path,
        incomplete_dir: &Path,
    ) -> Self {
        let http_port = free_port();
        let listen_port = free_port();

        let bind = |host: &Path, target: &str| {
            Mount::bind_mount(host.to_string_lossy().into_owned(), target.to_string())
        };
        let request = GenericImage::new("slskd/slskd", "latest")
            .with_wait_for(WaitFor::Nothing)
            .with_network("host")
            // No web auth/TLS: the test talks plain HTTP to the local API.
            .with_env_var("SLSKD_NO_AUTH", "true")
            .with_env_var("SLSKD_NO_HTTPS", "true")
            .with_env_var("SLSKD_HTTP_PORT", http_port.to_string())
            // Point slskd at our Soulfind instead of the real Soulseek network.
            .with_env_var("SLSKD_SLSK_ADDRESS", "127.0.0.1")
            .with_env_var("SLSKD_SLSK_PORT", soulfind_port.to_string())
            .with_env_var("SLSKD_SLSK_USERNAME", username)
            .with_env_var("SLSKD_SLSK_PASSWORD", password)
            .with_env_var("SLSKD_SLSK_LISTEN_PORT", listen_port.to_string())
            .with_env_var("SLSKD_SLSK_DIAG_LEVEL", "Debug")
            .with_env_var("SLSKD_SHARED_DIR", "/shared")
            .with_env_var("SLSKD_DOWNLOADS_DIR", "/downloads")
            .with_env_var("SLSKD_INCOMPLETE_DIR", "/incomplete")
            .with_mount(bind(shared_dir, "/shared"))
            .with_mount(bind(downloads_dir, "/downloads"))
            .with_mount(bind(incomplete_dir, "/incomplete"));

        let container = request.start().expect("starting slskd container (is Docker running?)");
        let slskd = Slskd {
            _container: container,
            base: format!("http://127.0.0.1:{http_port}"),
            username: username.to_string(),
            listen_port,
            downloads_dir: downloads_dir.to_path_buf(),
        };
        slskd.wait_until_logged_in();
        slskd
    }

    fn get_json(&self, path: &str) -> Result<serde_json::Value, String> {
        let body = ureq::get(&format!("{}{path}", self.base))
            .timeout(Duration::from_secs(20))
            .call()
            .map_err(|e| e.to_string())?
            .into_string()
            .map_err(|e| e.to_string())?;
        serde_json::from_str(&body).map_err(|e| format!("decoding {path}: {e}"))
    }

    /// Blocks until slskd's API is up and it reports a logged-in Soulseek
    /// session (Soulfind registers the username on this first login).
    fn wait_until_logged_in(&self) {
        let deadline = Instant::now() + Duration::from_secs(90);
        let mut last = String::from("(no response yet)");
        while Instant::now() < deadline {
            match self.get_json("/api/v0/application") {
                Ok(state) => {
                    if state["server"]["isLoggedIn"].as_bool().unwrap_or(false) {
                        return;
                    }
                    last = format!("server state = {}", state["server"]["state"]);
                }
                Err(err) => last = err,
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        panic!("slskd did not log into soulfind within 90s; last: {last}");
    }

    /// Browses a peer's shares and returns `(virtual_path, size)` pairs. The
    /// browse itself dials the peer, so it exercises peer connectivity.
    pub fn browse(&self, peer: &str) -> Result<Vec<(String, u64)>, String> {
        let value = self.get_json(&format!("/api/v0/users/{peer}/browse"))?;
        Ok(flatten_share_tree(&value))
    }

    /// Our own shared files as `(virtual_path, size)`; used to learn the exact
    /// path string a downloader must request.
    pub fn shares_contents(&self) -> Result<Vec<(String, u64)>, String> {
        let value = self.get_json("/api/v0/shares/contents")?;
        Ok(flatten_share_tree(&value))
    }

    /// Enqueues a download of `filename` (a peer-advertised virtual path) from
    /// `peer`.
    pub fn enqueue_download(&self, peer: &str, filename: &str, size: u64) -> Result<(), String> {
        let body = serde_json::json!([{ "filename": filename, "size": size }]).to_string();
        ureq::post(&format!("{}/api/v0/transfers/downloads/{peer}", self.base))
            .timeout(Duration::from_secs(20))
            .set("Content-Type", "application/json")
            .send_string(&body)
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// slskd's own container logs (stdout+stderr), for failure diagnostics.
    pub fn logs(&self) -> String {
        let out = self._container.stdout_to_vec().unwrap_or_default();
        let err = self._container.stderr_to_vec().unwrap_or_default();
        format!(
            "{}\n{}",
            String::from_utf8_lossy(&out),
            String::from_utf8_lossy(&err)
        )
    }

    /// Pretty-printed transfer + connection state, for failure diagnostics.
    pub fn debug_state(&self, peer: &str) -> String {
        let downloads = self
            .get_json(&format!("/api/v0/transfers/downloads/{peer}"))
            .map(|v| v.to_string())
            .unwrap_or_else(|e| format!("<error: {e}>"));
        let app = self
            .get_json("/api/v0/application")
            .map(|v| v["server"].to_string())
            .unwrap_or_else(|e| format!("<error: {e}>"));
        format!("slskd server={app}\nslskd downloads from {peer}={downloads}")
    }

    /// True once a download whose path ends in `basename` reached a succeeded
    /// state.
    pub fn download_succeeded(&self, peer: &str, basename: &str) -> Result<bool, String> {
        let value = self.get_json(&format!("/api/v0/transfers/downloads/{peer}"))?;
        Ok(transfer_files(&value).into_iter().any(|(name, state)| {
            name.replace('\\', "/").rsplit('/').next() == Some(basename)
                && state.contains("Succeeded")
        }))
    }
}

/// Flattens an slskd share/browse tree (`[{ name, files: [{ filename, size }] }]`,
/// or an object wrapping `directories`) into full `(path, size)` pairs. slskd
/// reports the directory in `name` and the file basename in `filename`; the
/// network path is `name\filename`.
fn flatten_share_tree(value: &serde_json::Value) -> Vec<(String, u64)> {
    let dirs = value
        .get("directories")
        .and_then(|d| d.as_array())
        .or_else(|| value.as_array());
    let mut out = Vec::new();
    for dir in dirs.into_iter().flatten() {
        let name = dir["name"].as_str().unwrap_or_default();
        for file in dir["files"].as_array().into_iter().flatten() {
            let Some(filename) = file["filename"].as_str() else { continue };
            let size = file["size"].as_u64().unwrap_or(0);
            let path = if name.is_empty() {
                filename.to_string()
            } else {
                format!("{name}\\{filename}")
            };
            out.push((path, size));
        }
    }
    out
}

/// Pulls `(filename, state)` pairs out of an slskd transfers response. The
/// all-users endpoint returns an array of users; the per-user endpoint
/// (`/transfers/downloads/{user}`) returns a single `{ directories, username }`
/// object — handle both.
fn transfer_files(value: &serde_json::Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let users: Vec<&serde_json::Value> = match value.as_array() {
        Some(array) => array.iter().collect(),
        None => vec![value],
    };
    for user in users {
        for dir in user["directories"].as_array().into_iter().flatten() {
            for file in dir["files"].as_array().into_iter().flatten() {
                let name = file["filename"].as_str().unwrap_or_default().to_string();
                let state = file["state"].as_str().unwrap_or_default().to_string();
                out.push((name, state));
            }
        }
    }
    out
}
