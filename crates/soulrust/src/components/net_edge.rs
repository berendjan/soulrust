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
use crate::messages::{HandlerId, NetConn, NetConnEvent, NetRx, NetTx};

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
        let server = &ctx.config.server;
        let server_addr = if server.username.trim().is_empty() {
            None
        } else {
            Some(format!("{}:{}", server.host, server.port))
        };
        let (write_tx, write_rx) = mpsc::channel();
        NetEdge { server_addr, write_rx, write_tx, sock: None }
    }
}

impl traits::core::Handler for NetEdge {
    type Id = HandlerId;
    const ID: HandlerId = HandlerId::NetEdge;

    fn on_start<W: traits::core::Writer>(&mut self, writer: &W) {
        let Some(addr) = self.server_addr.clone() else {
            Self::send(
                &NetConn {
                    event: NetConnEvent::Failed {
                        reason: "no soulseek username configured (see /config)".into(),
                    },
                },
                writer,
            );
            return;
        };

        let write_tx = self.write_tx.clone();
        let writer = writer.clone();
        std::thread::Builder::new()
            .name("soulrust-net".into())
            .spawn(move || connect_and_read(&addr, &write_tx, &writer))
            .expect("spawning network thread");
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
                            NetEdge::send(&NetRx { payload: payload.to_vec() }, writer);
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

/// Connection-event helper for the reader thread (which has no &self).
fn send_conn<W: traits::core::Writer>(event: NetConnEvent, writer: &W) {
    NetEdge::send(&NetConn { event }, writer);
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
