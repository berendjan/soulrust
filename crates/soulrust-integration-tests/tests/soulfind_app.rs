//! Full-application test: boots the real messenger (all workers, all
//! components, real sockets) against a Soulfind container and drives it the
//! way a user would — over HTTP.

use std::time::{Duration, Instant};

use rust_messenger::message_bus::atomic_circular_bus::CircularBus;
use rust_messenger::message_bus::condvar_bus::CondvarBus;
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

#[test]
fn app_logs_in_and_starts_searches_via_http() {
    let (_container, soulfind_port) = start_soulfind();
    let ui_port = free_port();

    let mut config = Config::default();
    config.server.host = "127.0.0.1".into();
    config.server.port = soulfind_port;
    config.server.username = unique_username("app");
    config.server.password = "app-secret".into();
    config.ui.bind_addr = format!("127.0.0.1:{ui_port}");
    config.update.enabled = false; // no GitHub calls from tests

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

    // Shut down cleanly via the same control surface main() uses.
    let _ = http_post_form(&format!("{base}/quit"), &[]);
    assert!(ctx.control.quit.load(std::sync::atomic::Ordering::Relaxed));
    messenger.stop();
    handles.join();
    std::fs::remove_file(&config_path).ok();
}
