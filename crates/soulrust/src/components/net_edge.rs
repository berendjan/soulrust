//! The Soulseek server socket edge: a connector/reader thread feeds decoded
//! frame payloads onto the bus as `NetRx`; the component's `NetTx` handler
//! writes outgoing frames to the socket. All protocol logic stays in the
//! session component.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::mpsc;
use std::time::Duration;

use rust_messenger::traits;
use rust_messenger::traits::extended::Sender;
use socket2::{SockRef, TcpKeepalive};
use soulseek_proto::frame::split_frame;

use crate::config::AppContext;
use crate::messages::{ConfigChanged, HandlerId, NetConn, NetConnKind, NetRx, NetTx};

fn server_addr(server: &crate::config::ServerConfig) -> Option<String> {
    if server.username.trim().is_empty() {
        None
    } else {
        Some(format!("{}:{}", server.host, server.port))
    }
}

pub struct NetEdge {
    /// host:port, or None when no credentials are configured (don't connect).
    server_addr: Option<String>,
    /// The connector thread hands the write half over through this channel.
    write_rx: mpsc::Receiver<TcpStream>,
    write_tx: mpsc::Sender<TcpStream>,
    sock: Option<TcpStream>,
}

impl NetEdge {
    pub fn new<W: traits::core::Writer>(ctx: &AppContext, _writer: &W) -> Self {
        let (write_tx, write_rx) = mpsc::channel();
        NetEdge { server_addr: server_addr(&ctx.config.server), write_rx, write_tx, sock: None }
    }

    /// Spawn the connector/reader thread for `addr`. It emits Connected/Closed/
    /// Failed and feeds NetRx; the session logs in on Connected.
    fn start_connector<W: traits::core::Writer>(&self, addr: String, writer: &W) {
        let write_tx = self.write_tx.clone();
        let writer = writer.clone();
        std::thread::Builder::new()
            .name("soulrust-net".into())
            .spawn(move || connect_and_read(&addr, &write_tx, &writer))
            .expect("spawning network thread");
    }

    /// Tear down any live connection: shutting down the socket stops the reader
    /// thread (its `read` returns 0), and we drop the write half + drain stale
    /// ones so the next NetTx doesn't pick up an old socket.
    fn disconnect(&mut self) {
        if let Some(sock) = self.sock.take() {
            let _ = sock.shutdown(std::net::Shutdown::Both);
        }
        while self.write_rx.try_recv().is_ok() {}
    }
}

impl traits::core::Handler for NetEdge {
    type Id = HandlerId;
    const ID: HandlerId = HandlerId::NetEdge;

    fn on_start<W: traits::core::Writer>(&mut self, writer: &W) {
        match self.server_addr.clone() {
            Some(addr) => self.start_connector(addr, writer),
            None => Self::send(
                &net_conn(NetConnEvent::Failed {
                        reason: "no soulseek username configured — set one in Settings".into(),
                    }),
                writer,
            ),
        }
    }
}

impl traits::core::Handle<ConfigChanged> for NetEdge {
    fn handle<W: traits::core::Writer>(&mut self, message: &ConfigChanged, writer: &W) {
        // Apply server credential/address changes live: drop any existing
        // connection and reconnect with the new settings (the session updates
        // its login from the same ConfigChanged and re-logs in on Connected).
        let new_addr = server_addr(&message.config.server);
        self.disconnect();
        self.server_addr = new_addr.clone();
        match new_addr {
            Some(addr) => self.start_connector(addr, writer),
            None => Self::send(
                &net_conn(NetConnEvent::Failed {
                        reason: "no soulseek username configured — set one in Settings".into(),
                    }),
                writer,
            ),
        }
    }
}

fn connect_and_read<W: traits::core::Writer>(
    addr: &str,
    write_tx: &mpsc::Sender<TcpStream>,
    writer: &W,
) {
    let stream = match TcpStream::connect(addr) {
        Ok(stream) => stream,
        Err(err) => {
            send_conn(NetConnEvent::Failed { reason: format!("connect {addr}: {err}") }, writer);
            return;
        }
    };

    // Best-effort socket tuning (non-fatal): like Nicotine+'s server connection,
    // disable Nagle for prompt control messages and enable TCP keepalive so a
    // silently dropped server link surfaces instead of hanging.
    if let Err(err) = configure_server_socket(&stream) {
        eprintln!("[net-edge] socket tuning failed: {err}");
    }

    match stream.try_clone() {
        Ok(write_half) => {
            // The handler picks this up on its first NetTx.
            let _ = write_tx.send(write_half);
        }
        Err(err) => {
            send_conn(
                NetConnEvent::Failed { reason: format!("clone socket: {err}") },
                writer,
            );
            return;
        }
    }
    send_conn(NetConnEvent::Connected, writer);

    let mut stream = stream;
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => {
                send_conn(
                    NetConnEvent::Closed { reason: "server closed the connection".into() },
                    writer,
                );
                return;
            }
            Ok(n) => {
                pending.extend_from_slice(&chunk[..n]);
                loop {
                    match split_frame(&pending) {
                        Ok(Some((payload, rest))) => {
                            NetEdge::send(&NetRx { payload: payload.to_vec(), ..Default::default() }, writer);
                            pending = rest.to_vec();
                        }
                        Ok(None) => break,
                        Err(err) => {
                            send_conn(
                                NetConnEvent::Closed { reason: format!("bad frame: {err}") },
                                writer,
                            );
                            return;
                        }
                    }
                }
            }
            Err(err) => {
                send_conn(NetConnEvent::Closed { reason: err.to_string() }, writer);
                return;
            }
        }
    }
}

/// Connection lifecycle, kept as a local enum for ergonomic construction and
/// mapped to the flat buffa `NetConn` (kind + reason) at the send boundary.
enum NetConnEvent {
    Connected,
    Failed { reason: String },
    Closed { reason: String },
}

fn net_conn(event: NetConnEvent) -> NetConn {
    match event {
        NetConnEvent::Connected => {
            NetConn { kind: NetConnKind::NetConnConnected.into(), ..Default::default() }
        }
        NetConnEvent::Failed { reason } => {
            NetConn { kind: NetConnKind::NetConnFailed.into(), reason, ..Default::default() }
        }
        NetConnEvent::Closed { reason } => {
            NetConn { kind: NetConnKind::NetConnClosed.into(), reason, ..Default::default() }
        }
    }
}

/// Connection-event helper for the reader thread (which has no &self).
fn send_conn<W: traits::core::Writer>(event: NetConnEvent, writer: &W) {
    NetEdge::send(&net_conn(event), writer);
}

/// Apply the server-connection socket options (TCP_NODELAY + keepalive),
/// mirroring the tuning Nicotine+ applies in its network thread.
fn configure_server_socket(stream: &TcpStream) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    let sock = SockRef::from(stream);
    sock.set_tcp_keepalive(&TcpKeepalive::new().with_time(Duration::from_secs(60)))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn configures_nodelay_and_keepalive_on_the_server_socket() {
        // Port of Nicotine+'s test_server_conn socket-option assertions: connect
        // to a local listener, apply the tuning, and confirm it took (nodelay is
        // the readable one; keepalive must at least apply without error).
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = TcpStream::connect(addr).unwrap();

        configure_server_socket(&stream).unwrap();

        assert!(stream.nodelay().unwrap(), "Nagle must be disabled on the server socket");
    }
}

impl traits::core::Handle<NetTx> for NetEdge {
    fn handle<W: traits::core::Writer>(&mut self, message: &NetTx, _writer: &W) {
        // Pick up a (re)connected write half if the connector produced one.
        while let Ok(stream) = self.write_rx.try_recv() {
            self.sock = Some(stream);
        }
        if let Some(sock) = &mut self.sock {
            // Write errors are surfaced by the reader thread as NetConn close.
            let _ = sock.write_all(&message.frame);
        }
    }
}
