//! Shared helpers for the Soulfind integration tests.
//!
//! These tests need Docker: they start the open-source Soulfind server
//! (`ghcr.io/soulfind-dev/soulfind`) via testcontainers and speak the real
//! protocol to it. They are tagged `docker` in BUILD.bazel and excluded from
//! default `bazel test //...` runs; use `bazel test --config=docker`.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use soulseek_proto::frame::split_frame;
use soulseek_proto::server::{LoginRequest, LoginResponse, ServerMessage, ServerRequest};
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::SyncRunner;
use testcontainers::{Container, GenericImage};

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
