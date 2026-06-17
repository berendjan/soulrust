//! Two-peer transfer test: boots Soulfind, the real soulrust app, and an
//! [slskd](https://github.com/slskd/slskd) peer — all logged into the same
//! server — then transfers a file in **both** directions and verifies the bytes
//! land on disk:
//!
//!   * slskd downloads a file shared by soulrust (exercises soulrust's upload
//!     path), driven over slskd's REST API.
//!   * soulrust downloads a file shared by slskd (exercises soulrust's download
//!     path), driven over soulrust's HTTP control surface.
//!
//! Peer-to-peer dialing only resolves correctly with Linux Docker networking
//! (slskd runs on the host network); hence the `docker`/`local`/
//! `requires-network` tags in BUILD.bazel. Run with `bazel test --config=docker`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rust_messenger::message_bus::atomic_circular_bus::CircularBus;
use rust_messenger::message_bus::condvar_bus::CondvarBus;
use soulrust::config::{AppContext, Config};
use soulrust_integration_tests::{free_port, start_soulfind, unique_username, Slskd};

const TRACK_BYTES: &[u8] = b"soulrust-shares-this-track-please-download-it";
const SONG_BYTES: &[u8] = b"slskd-shares-this-song-please-download-it-now";

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

/// Recursively finds a file by basename under `root` (download clients nest
/// transfers under per-user/per-directory folders).
fn find_file(root: &Path, basename: &str) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().and_then(|n| n.to_str()) == Some(basename) {
                return Some(path);
            }
        }
    }
    None
}

#[test]
fn transfers_a_file_both_ways_between_soulrust_and_slskd() {
    let (_soulfind, soulfind_port) = start_soulfind();

    // One temp tree holds both peers' shares + download dirs so cleanup is easy.
    let root = std::env::temp_dir().join(format!("soulrust-slskd-{}", std::process::id()));
    let sr_share = root.join("soulrust-share/Tunes");
    let sr_downloads = root.join("soulrust-downloads");
    let sr_incomplete = root.join("soulrust-incomplete");
    let slskd_share = root.join("slskd-share");
    let slskd_downloads = root.join("slskd-downloads");
    let slskd_incomplete = root.join("slskd-incomplete");
    for dir in [
        &sr_share,
        &sr_downloads,
        &sr_incomplete,
        &slskd_share.join("Album"),
        &slskd_downloads,
        &slskd_incomplete,
    ] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(sr_share.join("track.mp3"), TRACK_BYTES).unwrap();
    std::fs::write(slskd_share.join("Album").join("song.mp3"), SONG_BYTES).unwrap();

    // --- Boot the real soulrust app, sharing soulrust-share/Tunes -----------
    let ui_port = free_port();
    let listen_port = free_port();
    let sr_user = unique_username("soulrust");

    let mut config = Config::default();
    config.server.host = "127.0.0.1".into();
    config.server.port = soulfind_port;
    config.server.username = sr_user.clone();
    config.server.password = "soulrust-secret".into();
    config.server.listen_port = u32::from(listen_port);
    config.ui.bind_addr = format!("127.0.0.1:{ui_port}");
    config.update.enabled = false; // no GitHub calls from tests
    config.sharing.folders = vec![sr_share.to_string_lossy().into_owned()];
    config.sharing.download_dir = sr_downloads.to_string_lossy().into_owned();
    config.sharing.incomplete_dir = sr_incomplete.to_string_lossy().into_owned();

    let config_path = root.join("soulrust.yaml");
    let ctx = AppContext::new(config, config_path.clone());
    let bus = CondvarBus::new(CircularBus::new(&ctx));
    let messenger = soulrust::wiring::Messenger::new(bus);
    let handles = messenger.run(&ctx);
    let base = format!("http://127.0.0.1:{ui_port}");

    assert!(
        poll_until(Duration::from_secs(30), || {
            Ok(http_get(&format!("{base}/fragments/status"))?.contains("logged in as"))
        }),
        "soulrust never logged into soulfind"
    );

    // --- Boot slskd, sharing slskd-share, logged into the same server -------
    let slskd_user = unique_username("slskd");
    let slskd = Slskd::start(
        soulfind_port,
        &slskd_user,
        "slskd-secret",
        &slskd_share,
        &slskd_downloads,
        &slskd_incomplete,
    );

    // === Direction 1: slskd downloads the file soulrust shares =============
    // Browse exercises the peer connection and tells us the exact path
    // soulrust advertises.
    let mut track = None;
    assert!(
        poll_until(Duration::from_secs(30), || {
            match slskd.browse(&sr_user)?.into_iter().find(|(p, _)| p.ends_with("track.mp3")) {
                Some(found) => {
                    track = Some(found);
                    Ok(true)
                }
                None => Ok(false),
            }
        }),
        "slskd never saw soulrust's shared track.mp3 via browse"
    );
    let (track_path, track_size) = track.unwrap();
    assert_eq!(track_size, TRACK_BYTES.len() as u64, "advertised size mismatch");

    slskd.enqueue_download(&sr_user, &track_path, track_size).unwrap();
    if !poll_until(Duration::from_secs(60), || slskd.download_succeeded(&sr_user, "track.mp3")) {
        panic!(
            "slskd never completed its download from soulrust:\n{}",
            slskd.debug_state(&sr_user)
        );
    }
    let got = find_file(&slskd_downloads, "track.mp3")
        .expect("track.mp3 should be on disk in slskd's downloads dir");
    assert_eq!(
        std::fs::read(&got).unwrap(),
        TRACK_BYTES,
        "bytes slskd downloaded from soulrust must match what soulrust shared"
    );

    // === Direction 2: soulrust downloads the file slskd shares =============
    let (song_path, song_size) = slskd
        .shares_contents()
        .unwrap()
        .into_iter()
        .find(|(p, _)| p.ends_with("song.mp3"))
        .expect("slskd should be sharing song.mp3");
    assert_eq!(song_size, SONG_BYTES.len() as u64);

    let response = http_post_form(
        &format!("{base}/download"),
        &[
            ("username", slskd_user.as_str()),
            ("filename", song_path.as_str()),
            ("size", song_size.to_string().as_str()),
        ],
    )
    .unwrap();
    assert!(response.contains("queued"), "download not accepted: {response}");

    let dir2_ok = poll_until(Duration::from_secs(60), || {
        let status = http_get(&format!("{base}/fragments/status"))?;
        Ok(status.contains("downloaded") && status.contains("song.mp3"))
    });
    if !dir2_ok {
        eprintln!(
            "DIAG dir2 FAILED soulrust status: {}",
            http_get(&format!("{base}/fragments/status")).unwrap_or_default()
        );
        panic!("soulrust never reported completing the download from slskd");
    }
    let got = find_file(&sr_downloads, "song.mp3")
        .expect("song.mp3 should be on disk in soulrust's downloads dir");
    assert_eq!(
        std::fs::read(&got).unwrap(),
        SONG_BYTES,
        "bytes soulrust downloaded from slskd must match what slskd shared"
    );

    // --- Shut down cleanly --------------------------------------------------
    let _ = http_post_form(&format!("{base}/quit"), &[]);
    messenger.stop();
    handles.join();
    std::fs::remove_dir_all(&root).ok();
}
