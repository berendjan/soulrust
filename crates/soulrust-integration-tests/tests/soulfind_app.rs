//! Full-application test: boots the real messenger (all workers, all
//! components, real sockets) against a Soulfind container and drives it the
//! way a user would — over HTTP.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use rust_messenger::message_bus::atomic_circular_bus::CircularBus;
use rust_messenger::message_bus::condvar_bus::CondvarBus;
use soulseek_proto::frame::split_frame;
use soulseek_proto::peer::{ConnectionType, PeerInit};
use soulseek_proto::peer_message::{GetSharedFileList, PeerMessage, SharedFileListResponse};
use soulrust::config::{AppContext, Config};
use soulrust_integration_tests::{free_port, start_soulfind, unique_username};

fn http_get(url: &str) -> Result<String, String> {
    ureq::get(url)
        .timeout(Duration::from_secs(20))
        .call()
        .map_err(|e| e.to_string())?
        .into_string()
        .map_err(|e| e.to_string())
}

fn http_post_form(url: &str, form: &[(&str, &str)]) -> Result<String, String> {
    ureq::post(url)
        .timeout(Duration::from_secs(20))
        .send_form(form)
        .map_err(|e| e.to_string())?
        .into_string()
        .map_err(|e| e.to_string())
}

fn poll_until(deadline: Duration, mut probe: impl FnMut() -> Result<bool, String>) -> bool {
    let end = Instant::now() + deadline;
    let mut last_err = String::new();
    while Instant::now() < end {
        match probe() {
            Ok(true) => return true,
            Ok(false) => {}
            Err(err) => last_err = err,
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    eprintln!("poll_until timed out; last error: {last_err}");
    false
}

/// Acts as a peer: connect to `port`, send the peer-init + a browse request,
/// and decode the SharedFileListResponse the reactor serves back.
fn fetch_browse_from_peer(port: u16) -> Result<SharedFileListResponse, String> {
    let mut sock = TcpStream::connect(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    sock.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let init = PeerInit { username: "stub".into(), connection_type: ConnectionType::Peer, token: 0 };
    sock.write_all(&init.to_frame()).map_err(|e| e.to_string())?;
    sock.write_all(&GetSharedFileList.to_frame()).map_err(|e| e.to_string())?;

    let mut pending = Vec::new();
    let mut chunk = [0u8; 8192];
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some((payload, _rest)) = split_frame(&pending).map_err(|e| e.to_string())? {
            return match PeerMessage::decode(payload).map_err(|e| e.to_string())? {
                PeerMessage::SharedFileList(list) => Ok(list),
                other => Err(format!("unexpected peer message: {other:?}")),
            };
        }
        if Instant::now() > deadline {
            return Err("timed out reading the browse response".into());
        }
        match sock.read(&mut chunk) {
            Ok(0) => return Err("peer closed before responding".into()),
            Ok(n) => pending.extend_from_slice(&chunk[..n]),
            Err(e) => return Err(e.to_string()),
        }
    }
}

#[test]
fn app_logs_in_and_starts_searches_via_http() {
    let (_container, soulfind_port) = start_soulfind();
    let ui_port = free_port();
    let listen_port = free_port();

    // A shared folder so the serving side has something to browse.
    let share_root = std::env::temp_dir().join(format!("soulrust-app-share-{}", std::process::id()));
    std::fs::create_dir_all(share_root.join("Tunes")).unwrap();
    std::fs::write(share_root.join("Tunes").join("track.mp3"), b"audio-bytes").unwrap();

    let mut config = Config::default();
    config.server.host = "127.0.0.1".into();
    config.server.port = soulfind_port;
    config.server.username = unique_username("app");
    config.server.password = "app-secret".into();
    config.server.listen_port = u32::from(listen_port);
    config.ui.bind_addr = format!("127.0.0.1:{ui_port}");
    config.update.enabled = false; // no GitHub calls from tests
    config.sharing.folders = vec![share_root.join("Tunes").to_string_lossy().into_owned()];

    let config_path = std::env::temp_dir().join(format!(
        "soulrust-app-test-{}.yaml",
        std::process::id()
    ));
    let ctx = AppContext::new(config, config_path.clone());

    let bus = CondvarBus::new(CircularBus::new(&ctx));
    let messenger = soulrust::wiring::Messenger::new(bus);
    let handles = messenger.run(&ctx);

    let base = format!("http://127.0.0.1:{ui_port}");

    // Index page serves and wires htmx.
    assert!(
        poll_until(Duration::from_secs(20), || {
            Ok(http_get(&format!("{base}/"))?.contains("htmx.min.js"))
        }),
        "UI never came up"
    );

    // The session must log in to soulfind (status fragment polls the Ui).
    assert!(
        poll_until(Duration::from_secs(30), || {
            Ok(http_get(&format!("{base}/fragments/status"))?.contains("logged in as"))
        }),
        "session never reached logged-in state"
    );

    // Submitting a plain search goes extract -> session -> searches table.
    let response =
        http_post_form(&format!("{base}/search"), &[("input", "test artist - song")]).unwrap();
    assert!(
        response.contains("started 1 search(es)"),
        "unexpected search response: {response}"
    );
    assert!(response.contains("test artist - song"));

    // The searches fragment shows it on subsequent polls too.
    let fragment = http_get(&format!("{base}/fragments/searches")).unwrap();
    assert!(fragment.contains("test artist - song"));

    // Spotify URLs without credentials fail with a clear message, not a hang.
    let response = http_post_form(
        &format!("{base}/search"),
        &[("input", "https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC")],
    )
    .unwrap();
    assert!(
        response.contains("missing credentials"),
        "unexpected spotify response: {response}"
    );

    // Browsing exercises the full peer path: bridge -> session -> GetPeerAddress
    // -> (peer edge). An unknown user resolves to 0.0.0.0:0, so the session
    // reports the browse as failed — but the whole nano-service chain must run
    // and surface a clean outcome rather than hanging or staying blank.
    let response =
        http_post_form(&format!("{base}/browse"), &[("username", "nonexistent-peer-xyz")]).unwrap();
    assert!(
        response.contains("browsing"),
        "browse request should be accepted while logged in: {response}"
    );
    assert!(
        poll_until(Duration::from_secs(30), || {
            Ok(http_get(&format!("{base}/fragments/browse"))?.contains("nonexistent-peer-xyz"))
        }),
        "browse outcome never reached the browse fragment"
    );

    // Serving side: a peer connects to OUR listener and browses our shares.
    // (Retried because the reactor binds asynchronously on startup.)
    let mut listing: Option<SharedFileListResponse> = None;
    assert!(
        poll_until(Duration::from_secs(20), || {
            match fetch_browse_from_peer(listen_port) {
                Ok(list) => {
                    listing = Some(list);
                    Ok(true)
                }
                Err(err) => Err(err),
            }
        }),
        "a peer never managed to browse our shares"
    );
    let listing = listing.unwrap();
    let tunes = listing
        .directories
        .iter()
        .find(|dir| dir.path == "Tunes")
        .expect("our shared 'Tunes' folder should be browsable");
    assert!(
        tunes.files.iter().any(|f| f.name == "track.mp3"),
        "the shared file should be served (by basename) in the folder stream"
    );
    std::fs::remove_dir_all(&share_root).ok();

    // Shut down cleanly via the same control surface main() uses.
    let _ = http_post_form(&format!("{base}/quit"), &[]);
    assert!(ctx.control.quit.load(std::sync::atomic::Ordering::Relaxed));
    messenger.stop();
    handles.join();
    std::fs::remove_file(&config_path).ok();
}
