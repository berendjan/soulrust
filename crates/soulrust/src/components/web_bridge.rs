//! The HTTP edge: tiny_http worker threads serve the htmx UI by translating
//! requests into bus messages and waiting on per-request reply channels.
//!
//! Each request allocates a correlation id, registers an `mpsc::Sender` in
//! the shared pending map, sends the request message onto the bus, and
//! blocks (with a timeout) for the reply. The `WebBridge` component's
//! handlers complete those channels when the response messages come back.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use rust_messenger::traits;
use rust_messenger::traits::extended::Sender;

use crate::components::ui_theme::shell;
use crate::config::{AppContext, Config, Control};
use crate::extract::Job;
use crate::messages::{
    ApplyUpdateReq, ApplyUpdateResult, BrowseAccepted, BrowseHtml, BrowseRenderReq, BrowseUser,
    CancelDownload, ConfigSnapshot, ExtractRequest, ExtractResult, GetConfigReq, HandlerId,
    HttpHtml, HttpRender, Page, SetConfigReq, SetConfigResult, StartDownload, StartSearch,
    StartSearchResult, StartedSearch,
};

const REPLY_TIMEOUT: Duration = Duration::from_secs(15);
const HTTP_WORKERS: usize = 4;

/// Every reply the bridge can be waiting for, keyed by correlation id.
enum BridgeReply {
    Html(String),
    Extract(Result<Job, String>),
    Search { started: Vec<StartedSearch>, error: Option<String> },
    Config(Box<Config>),
    SetConfig(Result<(), String>),
    Apply(Result<(), String>),
    Browse(Option<String>),
}

type Pending = Arc<Mutex<HashMap<u64, mpsc::Sender<BridgeReply>>>>;

pub struct WebBridge {
    pending: Pending,
    corr: Arc<AtomicU64>,
    bind_addr: String,
    control: Arc<Control>,
}

impl WebBridge {
    pub fn new<W: traits::core::Writer>(ctx: &AppContext, _writer: &W) -> Self {
        WebBridge {
            pending: Arc::new(Mutex::new(HashMap::new())),
            corr: Arc::new(AtomicU64::new(0)),
            bind_addr: ctx.config.ui.bind_addr.clone(),
            control: ctx.control.clone(),
        }
    }

    fn complete(&self, corr: u64, reply: BridgeReply) {
        // A missing entry means the HTTP worker timed out; drop the reply.
        if let Some(tx) = self.pending.lock().unwrap().remove(&corr) {
            let _ = tx.send(reply);
        }
    }
}

impl traits::core::Handler for WebBridge {
    type Id = HandlerId;
    const ID: HandlerId = HandlerId::WebBridge;

    fn on_start<W: traits::core::Writer>(&mut self, writer: &W) {
        let server = match tiny_http::Server::http(&self.bind_addr) {
            Ok(server) => Arc::new(server),
            Err(err) => {
                eprintln!("error: cannot bind UI server on {}: {err}", self.bind_addr);
                return;
            }
        };
        println!("soulrust UI listening on http://{}", self.bind_addr);

        for n in 0..HTTP_WORKERS {
            let server = server.clone();
            let shared = SharedBridge {
                pending: self.pending.clone(),
                corr: self.corr.clone(),
                control: self.control.clone(),
                writer: writer.clone(),
            };
            std::thread::Builder::new()
                .name(format!("soulrust-http-{n}"))
                .spawn(move || loop {
                    match server.recv() {
                        Ok(request) => shared.handle_request(request),
                        Err(_) => return, // server dropped
                    }
                })
                .expect("spawning http worker thread");
        }
    }
}

// Response-message handlers: each completes the matching reply channel.

impl traits::core::Handle<HttpHtml> for WebBridge {
    fn handle<W: traits::core::Writer>(&mut self, message: &HttpHtml, _writer: &W) {
        self.complete(message.corr, BridgeReply::Html(message.html.clone()));
    }
}

impl traits::core::Handle<ExtractResult> for WebBridge {
    fn handle<W: traits::core::Writer>(&mut self, message: &ExtractResult, _writer: &W) {
        self.complete(message.corr, BridgeReply::Extract(message.result.clone()));
    }
}

impl traits::core::Handle<StartSearchResult> for WebBridge {
    fn handle<W: traits::core::Writer>(&mut self, message: &StartSearchResult, _writer: &W) {
        self.complete(
            message.corr,
            BridgeReply::Search { started: message.started.clone(), error: message.error.clone() },
        );
    }
}

impl traits::core::Handle<ConfigSnapshot> for WebBridge {
    fn handle<W: traits::core::Writer>(&mut self, message: &ConfigSnapshot, _writer: &W) {
        self.complete(message.corr, BridgeReply::Config(Box::new(message.config.clone())));
    }
}

impl traits::core::Handle<SetConfigResult> for WebBridge {
    fn handle<W: traits::core::Writer>(&mut self, message: &SetConfigResult, _writer: &W) {
        self.complete(message.corr, BridgeReply::SetConfig(message.result.clone()));
    }
}

impl traits::core::Handle<ApplyUpdateResult> for WebBridge {
    fn handle<W: traits::core::Writer>(&mut self, message: &ApplyUpdateResult, _writer: &W) {
        self.complete(message.corr, BridgeReply::Apply(message.result.clone()));
    }
}

impl traits::core::Handle<BrowseAccepted> for WebBridge {
    fn handle<W: traits::core::Writer>(&mut self, message: &BrowseAccepted, _writer: &W) {
        self.complete(message.corr, BridgeReply::Browse(message.error.clone()));
    }
}

impl traits::core::Handle<BrowseHtml> for WebBridge {
    fn handle<W: traits::core::Writer>(&mut self, message: &BrowseHtml, _writer: &W) {
        self.complete(message.corr, BridgeReply::Html(message.html.clone()));
    }
}

/// Everything an HTTP worker thread needs, cloneable per thread.
struct SharedBridge<W: traits::core::Writer> {
    pending: Pending,
    corr: Arc<AtomicU64>,
    control: Arc<Control>,
    writer: W,
}

impl<W: traits::core::Writer> SharedBridge<W> {
    /// Registers a reply channel, lets `send_msg` put the request on the bus,
    /// and blocks until the bridge component completes the channel.
    fn round_trip(&self, send_msg: impl FnOnce(u64)) -> Result<BridgeReply, String> {
        let corr = self.corr.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap().insert(corr, tx);
        send_msg(corr);
        let reply = rx.recv_timeout(REPLY_TIMEOUT);
        if reply.is_err() {
            self.pending.lock().unwrap().remove(&corr);
        }
        reply.map_err(|_| "timed out waiting for the app to respond".to_string())
    }

    fn render(&self, page: Page) -> Result<String, String> {
        match self.round_trip(|corr| {
            WebBridge::send(&HttpRender { corr, page: page.clone() }, &self.writer);
        })? {
            BridgeReply::Html(html) => Ok(html),
            _ => Err("unexpected reply type".into()),
        }
    }

    fn handle_request(&self, mut request: tiny_http::Request) {
        let method = request.method().clone();
        let url = request.url().to_string();
        let path = url.split('?').next().unwrap_or("/").to_string();

        let mut body = String::new();
        let _ = request.as_reader().read_to_string(&mut body);

        let (status, content_type, content): (u32, &str, Vec<u8>) =
            match (method.as_str(), path.as_str()) {
                ("GET", "/assets/htmx.min.js") => (
                    200,
                    "text/javascript",
                    include_bytes!("../../assets/htmx.min.js").to_vec(),
                ),
                ("GET", "/assets/app.js") => {
                    (200, "text/javascript", include_bytes!("../../assets/app.js").to_vec())
                }
                ("GET", "/") => self.html_page(self.render(Page::Index)),
                ("GET", "/fragments/status") => self.html_page(self.render(Page::StatusFragment)),
                ("GET", "/fragments/searches") => {
                    self.html_page(self.render(Page::SearchesFragment))
                }
                ("GET", "/fragments/browse") => self.html_page(self.browse_fragment()),
                ("GET", "/fragments/account-status") => {
                    self.html_page(self.render(Page::AccountStatus))
                }
                ("GET", "/account") => self.html_page(self.account_page(None)),
                ("GET", "/downloads") => self.html_page(self.render(Page::Downloads)),
                ("GET", "/fragments/downloads") => {
                    self.html_page(self.render(Page::DownloadsFragment))
                }
                ("GET", "/bulk") => self.html_page(self.bulk_page()),
                ("GET", "/spotify") => self.html_page(self.spotify_page(None)),
                ("GET", "/config") => self.html_page(self.config_page()),
                ("POST", "/account") => self.html_page(self.save_account(&body)),
                ("POST", "/download") => self.html_page(self.submit_download(&body)),
                ("POST", "/download/cancel") => self.html_page(self.cancel_download(&body)),
                ("POST", "/search") => self.html_page(self.submit_search(&body)),
                ("POST", "/filter") => self.html_page(self.filter_bitrate(&body)),
                ("GET", p) if p.starts_with("/sort/") => {
                    let key = p.trim_start_matches("/sort/").to_string();
                    self.html_page(self.render(Page::SortSearches { key }))
                }
                ("POST", "/browse") => self.html_page(self.submit_browse(&body)),
                ("POST", "/spotify") => self.html_page(self.save_spotify(&body)),
                ("POST", "/config") => self.html_page(self.save_config(&body)),
                ("POST", "/apply-update") => self.html_page(self.apply_update()),
                ("POST", "/restart") => {
                    self.control.restart.store(true, Ordering::Relaxed);
                    (200, "text/html", b"restarting soulrust...".to_vec())
                }
                ("POST", "/quit") => {
                    self.control.quit.store(true, Ordering::Relaxed);
                    (200, "text/html", b"goodbye".to_vec())
                }
                _ => (404, "text/html", b"<h1>404</h1>".to_vec()),
            };

        let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes())
            .expect("static header");
        let response = tiny_http::Response::from_data(content)
            .with_status_code(status)
            .with_header(header);
        let _ = request.respond(response);
    }

    fn html_page(&self, result: Result<String, String>) -> (u32, &'static str, Vec<u8>) {
        match result {
            Ok(html) => (200, "text/html", html.into_bytes()),
            Err(err) => (
                504,
                "text/html",
                format!(r#"<div class="banner error">{}</div>"#, escape(&err)).into_bytes(),
            ),
        }
    }

    /// POST /search: extract the input into jobs, start the searches, then
    /// return the refreshed searches fragment (with an error banner if any
    /// step failed).
    fn submit_search(&self, body: &str) -> Result<String, String> {
        let form = parse_form(body);
        let input = form.get("input").cloned().unwrap_or_default();

        let extract = match self.round_trip(|corr| {
            WebBridge::send(&ExtractRequest { corr, input: input.clone() }, &self.writer);
        })? {
            BridgeReply::Extract(result) => result,
            _ => return Err("unexpected reply type".into()),
        };

        let job = match extract {
            Ok(job) => job,
            Err(err) => {
                let fragment = self.render(Page::SearchesFragment)?;
                return Ok(format!(
                    r#"<div class="banner error">{}</div>{fragment}"#,
                    escape(&err)
                ));
            }
        };

        let search = match self.round_trip(|corr| {
            WebBridge::send(
                &StartSearch {
                    corr,
                    source_label: job.source_label.clone(),
                    jobs: job.searches.clone(),
                },
                &self.writer,
            );
        })? {
            BridgeReply::Search { started, error } => (started, error),
            _ => return Err("unexpected reply type".into()),
        };

        let banner = match search {
            (_, Some(error)) => {
                format!(r#"<div class="banner error">{}</div>"#, escape(&error))
            }
            (started, None) => format!(
                r#"<div class="banner">{} — started {} search(es)</div>"#,
                escape(&job.source_label),
                started.len()
            ),
        };
        let fragment = self.render(Page::SearchesFragment)?;
        Ok(format!("{banner}{fragment}"))
    }

    /// POST /filter: set the minimum-bitrate filter (kbps; blank/0 clears it)
    /// and return the refreshed results fragment.
    fn filter_bitrate(&self, body: &str) -> Result<String, String> {
        let min = parse_form(body)
            .get("min_bitrate")
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(0);
        self.render(Page::FilterBitrate { min })
    }

    /// POST /download: queue a file from a search result. Fire-and-forget —
    /// the session resolves the peer's address and peer_net opens the file
    /// connection; progress shows up in the activity log. We just swap the
    /// row's button for a confirmation.
    fn submit_download(&self, body: &str) -> Result<String, String> {
        let form = parse_form(body);
        let username = form.get("username").cloned().unwrap_or_default();
        let filename = form.get("filename").cloned().unwrap_or_default();
        let size = form.get("size").and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        if username.is_empty() || filename.is_empty() {
            return Ok(r#"<span class="pill warn">bad request</span>"#.into());
        }
        WebBridge::send(&StartDownload { username, filename, size }, &self.writer);
        // Matches how the polled row renders a queued download, so it doesn't
        // visibly change when the next 2s refresh lands.
        Ok(r#"<span class="pill">queued</span>"#.into())
    }

    /// POST /download/cancel: drop a queued/active download. We tell the UI and
    /// peer_net to forget it, then return a fresh Get button — the search row
    /// swaps its action cell back to it, while the Downloads page removes the row
    /// (it uses hx-swap="delete" and ignores this body).
    fn cancel_download(&self, body: &str) -> Result<String, String> {
        let form = parse_form(body);
        let username = form.get("username").cloned().unwrap_or_default();
        let filename = form.get("filename").cloned().unwrap_or_default();
        let size = form.get("size").and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        WebBridge::send(
            &CancelDownload { username: username.clone(), filename: filename.clone() },
            &self.writer,
        );
        Ok(format!(
            r##"<form hx-post="/download" hx-target="this" hx-swap="outerHTML" style="margin:0"><input type="hidden" name="username" value="{user}"><input type="hidden" name="filename" value="{path}"><input type="hidden" name="size" value="{size}"><button class="btn xs" type="submit">Get</button></form>"##,
            user = escape(&username),
            path = escape(&filename),
            size = size,
        ))
    }

    /// GET /fragments/browse: render the current browse state (owned by the
    /// Browse read-model component).
    fn browse_fragment(&self) -> Result<String, String> {
        match self.round_trip(|corr| {
            WebBridge::send(&BrowseRenderReq { corr }, &self.writer);
        })? {
            BridgeReply::Html(html) => Ok(html),
            _ => Err("unexpected reply type".into()),
        }
    }

    /// POST /browse: ask the session to browse a user, then return a banner
    /// plus the current browse fragment (the listing itself arrives later and
    /// shows up on the next poll).
    fn submit_browse(&self, body: &str) -> Result<String, String> {
        let form = parse_form(body);
        let username = form.get("username").cloned().unwrap_or_default();

        let error = match self.round_trip(|corr| {
            WebBridge::send(&BrowseUser { corr, username: username.clone() }, &self.writer);
        })? {
            BridgeReply::Browse(error) => error,
            _ => return Err("unexpected reply type".into()),
        };

        let fragment = self.browse_fragment()?;
        let banner = match error {
            Some(error) => format!(r#"<div class="banner error">{}</div>"#, escape(&error)),
            None => format!(
                r#"<div class="banner">browsing {}… results will appear below</div>"#,
                escape(username.trim())
            ),
        };
        Ok(format!("{banner}{fragment}"))
    }

    fn current_config(&self) -> Result<Config, String> {
        match self.round_trip(|corr| {
            WebBridge::send(&GetConfigReq { corr }, &self.writer);
        })? {
            BridgeReply::Config(config) => Ok(*config),
            _ => Err("unexpected reply type".into()),
        }
    }

    fn config_page(&self) -> Result<String, String> {
        let config = self.current_config()?;
        Ok(render_config_page(&config, None))
    }

    fn save_config(&self, body: &str) -> Result<String, String> {
        let form = parse_form(body);
        let mut config = self.current_config()?;

        let get = |key: &str| form.get(key).map(|s| s.trim().to_owned());
        if let Some(v) = get("host") {
            config.server.host = v;
        }
        if let Some(v) = get("port") {
            config.server.port = v.parse().map_err(|_| format!("invalid port: {v}"))?;
        }
        if let Some(v) = get("username") {
            config.server.username = v;
        }
        if let Some(v) = get("password") {
            if !v.is_empty() {
                config.server.password = v;
            }
        }
        if let Some(v) = get("listen_port") {
            config.server.listen_port =
                v.parse().map_err(|_| format!("invalid listen port: {v}"))?;
        }
        config.spotify.client_id = get("spotify_client_id").filter(|v| !v.is_empty());
        if let Some(v) = get("spotify_client_secret") {
            if !v.is_empty() {
                config.spotify.client_secret = Some(v);
            }
        }
        if let Some(v) = get("download_dir") {
            config.sharing.download_dir = v;
        }
        if let Some(v) = get("incomplete_dir") {
            config.sharing.incomplete_dir = v;
        }
        if let Some(v) = form.get("folders") {
            config.sharing.folders = v
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_owned)
                .collect();
        }
        if let Some(v) = get("upload_slots") {
            config.sharing.upload_slots =
                v.parse().map_err(|_| format!("invalid upload slots: {v}"))?;
        }
        config.sharing.respond_to_searches = form.contains_key("respond_to_searches");
        config.update.enabled = form.contains_key("update_enabled");
        config.update.auto_apply = form.contains_key("update_auto_apply");
        if let Some(v) = get("update_repo") {
            config.update.repo = v;
        }
        if let Some(v) = get("bind_addr") {
            config.ui.bind_addr = v;
        }
        if let Some(v) = get("min_result_files") {
            config.sharing.min_result_files =
                v.parse().map_err(|_| format!("invalid minimum files: {v}"))?;
        }
        if let Some(v) = get("min_peer_upload_speed") {
            config.sharing.min_peer_upload_speed =
                v.parse().map_err(|_| format!("invalid minimum upload speed: {v}"))?;
        }
        if let Some(v) = get("max_peer_queue_length") {
            config.sharing.max_peer_queue_length =
                v.parse().map_err(|_| format!("invalid maximum queue length: {v}"))?;
        }
        if let Some(v) = get("max_download_speed") {
            config.sharing.max_download_speed =
                v.parse().map_err(|_| format!("invalid max download speed: {v}"))?;
        }
        if let Some(v) = get("max_upload_speed") {
            config.sharing.max_upload_speed =
                v.parse().map_err(|_| format!("invalid max upload speed: {v}"))?;
        }

        let result = match self.round_trip(|corr| {
            WebBridge::send(&SetConfigReq { corr, config: config.clone() }, &self.writer);
        })? {
            BridgeReply::SetConfig(result) => result,
            _ => return Err("unexpected reply type".into()),
        };

        let banner = match &result {
            Ok(()) => r#"<div class="banner">Configuration saved — reconnecting to Soulseek with the new settings. Watch the status on the <a href="/">Search</a> page.</div>"#.to_string(),
            Err(err) => format!(r#"<div class="banner error">{}</div>"#, escape(err)),
        };
        Ok(render_config_page(&config, Some(banner)))
    }

    fn apply_update(&self) -> Result<String, String> {
        let result = match self.round_trip(|corr| {
            WebBridge::send(&ApplyUpdateReq { corr }, &self.writer);
        })? {
            BridgeReply::Apply(result) => result,
            _ => return Err("unexpected reply type".into()),
        };
        Ok(match result {
            Ok(()) => r#"<div class="banner">update installed — restart soulrust</div>"#.into(),
            Err(err) => format!(r#"<div class="banner error">{}</div>"#, escape(&err)),
        })
    }

    /// GET /account: the login / sign-up screen (reached from the footer chip).
    fn account_page(&self, banner: Option<String>) -> Result<String, String> {
        let config = self.current_config()?;
        Ok(render_account_page(&config, banner))
    }

    /// POST /account: save the Soulseek username/password, which triggers a live
    /// reconnect; the page then polls the connection status.
    fn save_account(&self, body: &str) -> Result<String, String> {
        let form = parse_form(body);
        let mut config = self.current_config()?;

        if let Some(v) = form.get("username").map(|s| s.trim().to_owned()) {
            config.server.username = v;
        }
        if let Some(v) = form.get("password") {
            if !v.is_empty() {
                config.server.password = v.clone();
            }
        }

        if config.server.username.trim().is_empty() {
            return Ok(render_account_page(
                &config,
                Some(r#"<div class="banner error">Enter a username to sign in or create an account.</div>"#.to_string()),
            ));
        }

        let result = match self.round_trip(|corr| {
            WebBridge::send(&SetConfigReq { corr, config: config.clone() }, &self.writer);
        })? {
            BridgeReply::SetConfig(result) => result,
            _ => return Err("unexpected reply type".into()),
        };

        let banner = match &result {
            Ok(()) => r#"<div class="banner">Signing in to Soulseek… the status below updates in a moment.</div>"#.to_string(),
            Err(err) => format!(r#"<div class="banner error">{}</div>"#, escape(err)),
        };
        Ok(render_account_page(&config, Some(banner)))
    }

    /// GET /bulk: a dedicated page for queuing many tracks at once. Pick a
    /// source, paste a Spotify link or a track list, and start the searches.
    fn bulk_page(&self) -> Result<String, String> {
        let config = self.current_config()?;
        Ok(render_bulk_page(&config))
    }

    /// GET /spotify: how to connect Spotify (create an app, get keys) plus the
    /// credential form. `banner` is shown after a save.
    fn spotify_page(&self, banner: Option<String>) -> Result<String, String> {
        let config = self.current_config()?;
        Ok(render_spotify_page(&config, banner))
    }

    /// POST /spotify: save just the Spotify credentials, then re-render the page.
    fn save_spotify(&self, body: &str) -> Result<String, String> {
        let form = parse_form(body);
        let mut config = self.current_config()?;

        config.spotify.client_id =
            form.get("spotify_client_id").map(|s| s.trim().to_owned()).filter(|v| !v.is_empty());
        if let Some(v) = form.get("spotify_client_secret").map(|s| s.trim().to_owned()) {
            if !v.is_empty() {
                config.spotify.client_secret = Some(v);
            }
        }

        let result = match self.round_trip(|corr| {
            WebBridge::send(&SetConfigReq { corr, config: config.clone() }, &self.writer);
        })? {
            BridgeReply::SetConfig(result) => result,
            _ => return Err("unexpected reply type".into()),
        };

        let banner = match &result {
            Ok(()) => {
                r#"<div class="banner">Spotify credentials saved.</div>"#.to_string()
            }
            Err(err) => format!(r#"<div class="banner error">{}</div>"#, escape(err)),
        };
        Ok(render_spotify_page(&config, Some(banner)))
    }
}

/// A placeholder of one bullet per character for a configured secret, so the
/// field shows that something is set without revealing it. `if_unset` is the
/// hint shown when there's no secret yet. Capped so a long secret can't blow
/// out the field width.
fn secret_placeholder(secret: &str, if_unset: &str) -> String {
    let len = secret.chars().count();
    if len == 0 {
        if_unset.to_string()
    } else {
        "•".repeat(len.min(40))
    }
}

/// True when both Spotify credentials are present.
fn spotify_connected(config: &Config) -> bool {
    config.spotify.client_id.as_deref().is_some_and(|s| !s.is_empty())
        && config.spotify.client_secret.as_deref().is_some_and(|s| !s.is_empty())
}

/// A status pill + "set up" button for Spotify, shown on the bulk + spotify
/// pages.
fn spotify_status_card(config: &Config) -> String {
    if spotify_connected(config) {
        r#"<div class="card"><span class="pill ok">● Spotify connected</span>
<a class="btn secondary" style="margin-left:0.6rem" href="/spotify">Manage</a></div>"#
            .to_string()
    } else {
        r#"<div class="card"><span class="pill warn">● Spotify not connected</span>
<a class="btn spotify" style="margin-left:0.6rem" href="/spotify">Set up Spotify</a>
<p class="muted" style="margin:0.6rem 0 0">Connect Spotify to paste playlist, album, and track links.</p></div>"#
            .to_string()
    }
}

fn render_bulk_page(config: &Config) -> String {
    let body = format!(
        r##"<h1>Bulk downloads</h1>
<p class="sub">Queue many tracks at once — paste a Spotify link or a plain track list, and soulrust searches the network for each.</p>
{spotify_card}
<div class="card">
<form hx-post="/search" hx-target="#bulk-results" hx-swap="innerHTML">
  <label for="source">Source</label>
  <select id="source" name="source">
    <option value="spotify">Spotify link (playlist / album / track)</option>
    <option value="tracklist">Track list (one "artist - title" per line)</option>
  </select>
  <label for="input" style="margin-top:0.9rem">Paste here</label>
  <textarea id="input" name="input" placeholder="https://open.spotify.com/playlist/...&#10;— or —&#10;Daft Punk - Get Lucky&#10;Justice - Genesis"></textarea>
  <p class="muted" style="margin:0.5rem 0 0.9rem">Spotify links need credentials set up first. Plain track lists work with no setup.</p>
  <button class="btn" type="submit">Start searches</button>
</form>
</div>
<h2>Searches</h2>
{col_bar}
<div id="bulk-results" class="results" hx-get="/fragments/searches" hx-trigger="load, every 2s"></div>"##,
        col_bar = crate::components::ui_theme::col_bar(0),
        spotify_card = spotify_status_card(config),
    );
    shell("soulrust — bulk downloads", "bulk", &config.server.username, &body)
}

fn render_spotify_page(config: &Config, banner: Option<String>) -> String {
    let status = if spotify_connected(config) {
        r#"<span class="pill ok">● Connected</span>"#
    } else {
        r#"<span class="pill warn">● Not connected</span>"#
    };
    let body = format!(
        r#"<h1>Connect Spotify</h1>
<p class="sub">{status} &nbsp; soulrust reads <strong>public</strong> Spotify playlists, albums, and tracks to turn them into searches. You'll create a free Spotify app once and paste its two keys below.</p>
{banner}
<div class="card">
<h2 style="margin-top:0">Get your keys (about 2 minutes)</h2>
<ol class="steps">
  <li>Open the <a href="https://developer.spotify.com/dashboard" target="_blank" rel="noopener">Spotify Developer Dashboard</a> and log in with any Spotify account (a free account works).</li>
  <li>Click <strong>Create app</strong>.</li>
  <li>Fill in an <strong>App name</strong> (e.g. <code>soulrust</code>) and a short description. For <strong>Redirect URI</strong> enter <code>http://127.0.0.1/callback</code> — soulrust never uses it, but Spotify's form won't save without one. Under "Which API/SDKs…?" tick <strong>Web API</strong>, accept the terms, and click <strong>Save</strong>.</li>
  <li>Open your new app and go to <strong>Settings</strong>.</li>
  <li>Copy the <strong>Client ID</strong>. Click <strong>View client secret</strong> and copy the <strong>Client secret</strong>.</li>
  <li>Paste both below and click <strong>Save</strong>. That's it.</li>
</ol>
<p class="muted">This uses Spotify's "Client Credentials" mode: it reads public playlists/albums/tracks only, and never logs into or touches your account, so your private and liked songs aren't visible.</p>
</div>
<div class="card">
<form hx-post="/spotify" hx-target="body">
  <label>Client ID <input type="text" name="spotify_client_id" value="{client_id}" placeholder="e.g. 1a2b3c4d5e6f..."></label>
  <label>Client secret <input type="password" name="spotify_client_secret" value="" placeholder="{secret_ph}"></label>
  <p style="margin-top:0.9rem"><button class="btn spotify" type="submit">Save</button>
  <a class="btn secondary" style="margin-left:0.5rem" href="/bulk">Back to bulk downloads</a></p>
</form>
</div>"#,
        status = status,
        banner = banner.unwrap_or_default(),
        client_id = escape(config.spotify.client_id.as_deref().unwrap_or("")),
        secret_ph = secret_placeholder(
            config.spotify.client_secret.as_deref().unwrap_or(""),
            "paste the client secret",
        ),
    );
    shell("soulrust — connect Spotify", "spotify", &config.server.username, &body)
}

fn render_account_page(config: &Config, banner: Option<String>) -> String {
    let body = format!(
        r##"<h1>Account</h1>
<p class="sub">Sign in to Soulseek, or create a new account. Soulseek has no separate sign-up — entering a username nobody has used yet registers it on first sign-in.</p>
{banner}
<div id="account-status" hx-get="/fragments/account-status" hx-trigger="load, every 2s"></div>
<div class="card">
<form hx-post="/account" hx-target="body">
  <label for="acc-user">Username</label>
  <input id="acc-user" type="text" name="username" value="{username}" placeholder="your Soulseek username" autocomplete="username">
  <label for="acc-pass" style="margin-top:0.8rem">Password</label>
  <input id="acc-pass" type="password" name="password" value="" placeholder="{password_ph}" autocomplete="current-password">
  <p style="margin-top:1rem"><button class="btn" type="submit">Sign in / create account</button></p>
</form>
</div>
<div class="card">
<h2 style="margin-top:0">Creating a new account</h2>
<ol class="steps">
  <li>Pick a <strong>username nobody else has used</strong> and any password.</li>
  <li>Click <strong>Sign in / create account</strong> — the name is registered on first sign-in.</li>
  <li>If it says <code>INVALIDPASS</code>, that username is already taken — choose a different one.</li>
</ol>
<p class="muted">There's no email or password recovery on Soulseek, so keep your password safe. It's stored locally and only ever sent to the server as a hash.</p>
</div>"##,
        banner = banner.unwrap_or_default(),
        username = escape(&config.server.username),
        password_ph = secret_placeholder(&config.server.password, "your password"),
    );
    shell("soulrust — account", "account", &config.server.username, &body)
}

fn render_config_page(config: &Config, banner: Option<String>) -> String {
    let checked = |b: bool| if b { "checked" } else { "" };
    let body = format!(
        r#"<h1>Settings</h1>
<p class="sub">Server, Spotify, updates, and the web UI address. Server and Spotify changes apply after a restart.</p>
<div id="result">{banner}</div>
<form hx-post="/config" hx-target="body">
<div class="card"><h2 style="margin-top:0">Soulseek server</h2>
<label>host <input type="text" name="host" value="{host}"></label>
<label>port <input type="text" name="port" value="{port}"></label>
<label>username <input type="text" name="username" value="{username}"></label>
<label>password (leave empty to keep) <input type="password" name="password" value="" placeholder="{password_ph}"></label>
<label>listen port <input type="text" name="listen_port" value="{listen_port}"></label>
</div>
<div class="card"><h2 style="margin-top:0">Downloads &amp; sharing</h2>
<p class="muted" style="margin-top:0">Where finished downloads land, and which of your folders you share with the network. Applied live — no restart needed.</p>
<label>download folder <input type="text" name="download_dir" value="{download_dir}" placeholder="{download_default}"></label>
<label>incomplete folder (optional) <input type="text" name="incomplete_dir" value="{incomplete_dir}" placeholder="{incomplete_default}"></label>
<label>shared folders — one path per line <textarea name="folders" rows="3" placeholder="/home/you/Music">{folders}</textarea></label>
<label>upload slots <input type="text" name="upload_slots" value="{upload_slots}"></label>
<label><input type="checkbox" name="respond_to_searches" {respond}> let other users find and download my shared files</label>
</div>
<div class="card"><h2 style="margin-top:0">Spotify</h2>
<p class="muted" style="margin-top:0">Prefer the guided <a href="/spotify">Connect Spotify</a> page if you're not sure how to get these.</p>
<label>client id <input type="text" name="spotify_client_id" value="{client_id}"></label>
<label>client secret (leave empty to keep) <input type="password" name="spotify_client_secret" value="" placeholder="{secret_ph}"></label>
</div>
<div class="card"><h2 style="margin-top:0">Updates</h2>
<label><input type="checkbox" name="update_enabled" {enabled}> check for updates on startup</label>
<label><input type="checkbox" name="update_auto_apply" {auto_apply}> apply automatically</label>
<label>github repo <input type="text" name="update_repo" value="{repo}"></label>
</div>
<div class="card"><h2 style="margin-top:0">Web UI</h2>
<label>bind address <input type="text" name="bind_addr" value="{bind_addr}"></label>
</div>
<div class="card"><h2 style="margin-top:0">Search result filters</h2>
<p class="muted" style="margin-top:0">Drop weak responses to your searches before they reach the results list.</p>
<label>minimum files per result <input type="text" name="min_result_files" value="{min_files}"></label>
<label>minimum peer upload speed (B/s, 0 = any) <input type="text" name="min_peer_upload_speed" value="{min_speed}"></label>
<label>maximum peer queue length (0 = no limit) <input type="text" name="max_peer_queue_length" value="{max_queue}"></label>
</div>
<div class="card"><h2 style="margin-top:0">Bandwidth limits</h2>
<p class="muted" style="margin-top:0">Aggregate throttles across all transfers, in bytes/second (0 = unlimited). Applied live.</p>
<label>max download speed (B/s, 0 = unlimited) <input type="text" name="max_download_speed" value="{max_down}"></label>
<label>max upload speed (B/s, 0 = unlimited) <input type="text" name="max_upload_speed" value="{max_up}"></label>
</div>
<p><button class="btn" type="submit">Save</button></p>
</form>"#,
        banner = banner.unwrap_or_default(),
        host = escape(&config.server.host),
        port = config.server.port,
        username = escape(&config.server.username),
        listen_port = config.server.listen_port,
        client_id = escape(config.spotify.client_id.as_deref().unwrap_or("")),
        password_ph = secret_placeholder(&config.server.password, ""),
        secret_ph = secret_placeholder(
            config.spotify.client_secret.as_deref().unwrap_or(""),
            "",
        ),
        download_dir = escape(&config.sharing.download_dir),
        incomplete_dir = escape(&config.sharing.incomplete_dir),
        download_default = escape(&config.sharing.download_path().display().to_string()),
        incomplete_default = escape(&config.sharing.incomplete_path().display().to_string()),
        folders = escape(&config.sharing.folders.join("\n")),
        upload_slots = config.sharing.upload_slots,
        respond = checked(config.sharing.respond_to_searches),
        enabled = checked(config.update.enabled),
        auto_apply = checked(config.update.auto_apply),
        repo = escape(&config.update.repo),
        bind_addr = escape(&config.ui.bind_addr),
        min_files = config.sharing.min_result_files,
        min_speed = config.sharing.min_peer_upload_speed,
        max_queue = config.sharing.max_peer_queue_length,
        max_down = config.sharing.max_download_speed,
        max_up = config.sharing.max_upload_speed,
    );
    shell("soulrust — settings", "config", &config.server.username, &body)
}

/// Minimal application/x-www-form-urlencoded parser (avoids a url dep).
fn parse_form(body: &str) -> HashMap<String, String> {
    body.split('&')
        .filter_map(|pair| {
            let mut kv = pair.splitn(2, '=');
            let key = percent_decode(kv.next()?);
            let value = percent_decode(kv.next().unwrap_or(""));
            if key.is_empty() {
                None
            } else {
                Some((key, value))
            }
        })
        .collect()
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => {
                let decoded = bytes
                    .get(i + 1..i + 3)
                    .and_then(|hex| std::str::from_utf8(hex).ok())
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok());
                match decoded {
                    Some(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_parsing_decodes_percent_and_plus() {
        let form = parse_form("input=hello+world%21&empty=&host=a%2Fb");
        assert_eq!(form["input"], "hello world!");
        assert_eq!(form["empty"], "");
        assert_eq!(form["host"], "a/b");
    }

    #[test]
    fn malformed_percent_sequences_pass_through() {
        let form = parse_form("a=100%&b=%zz&c=%2");
        assert_eq!(form["a"], "100%");
        assert_eq!(form["b"], "%zz");
        assert_eq!(form["c"], "%2");
    }

    #[test]
    fn config_page_renders_without_secrets() {
        let mut config = Config::default();
        config.server.password = "hunter2".into();
        config.spotify.client_secret = Some("sssh".into());
        let html = render_config_page(&config, None);
        assert!(!html.contains("hunter2"));
        assert!(!html.contains("sssh"));
        assert!(html.contains("server.slsknet.org"));
        // Shares the themed shell + nav.
        assert!(html.contains(r#"href="/bulk""#) && html.contains(r#"href="/spotify""#));
    }

    #[test]
    fn config_page_exposes_download_dir_and_shared_folders() {
        let mut config = Config::default();
        config.sharing.download_dir = "/home/me/Music/soulrust".into();
        config.sharing.folders = vec!["/home/me/Music".into(), "/data/flac".into()];
        let html = render_config_page(&config, None);
        assert!(html.contains(r#"name="download_dir""#) && html.contains("/home/me/Music/soulrust"));
        assert!(html.contains(r#"name="incomplete_dir""#));
        // Shared folders render one path per line inside the textarea.
        assert!(html.contains(r#"name="folders""#));
        assert!(html.contains("/home/me/Music\n/data/flac"));
        assert!(html.contains(r#"name="respond_to_searches""#));
    }

    #[test]
    fn configured_secrets_show_as_dots_not_values() {
        let mut config = Config::default();
        config.server.password = "hunter2".into(); // 7 chars
        config.spotify.client_secret = Some("abcd".into()); // 4 chars
        let html = render_config_page(&config, None);
        // The dot placeholders appear, one bullet per character...
        assert!(html.contains(&format!(r#"placeholder="{}""#, "•".repeat(7))));
        assert!(html.contains(&format!(r#"placeholder="{}""#, "•".repeat(4))));
        // ...and the real secrets are never rendered.
        assert!(!html.contains("hunter2") && !html.contains("abcd"));
    }

    #[test]
    fn unset_secret_has_no_dot_placeholder() {
        assert_eq!(secret_placeholder("", "type it"), "type it");
        assert_eq!(secret_placeholder("ab", "type it"), "••");
    }

    #[test]
    fn account_page_is_a_login_signup_screen() {
        let mut config = Config::default();
        config.server.username = "dj".into();
        config.server.password = "pw".into();
        let html = render_account_page(&config, None);
        // Username + password form posting to /account, with a live status region.
        assert!(html.contains(r#"name="username""#) && html.contains(r#"name="password""#));
        assert!(html.contains(r#"hx-post="/account""#));
        assert!(html.contains(r#"hx-get="/fragments/account-status""#));
        // Sign-up guidance, and the username is prefilled while the password is masked.
        assert!(html.contains("Creating a new account") && html.contains("INVALIDPASS"));
        assert!(html.contains(r#"value="dj""#));
        assert!(!html.contains(r#"value="pw""#), "password is never rendered");
    }

    #[test]
    fn bulk_page_has_a_source_selector_and_spotify_setup() {
        let html = render_bulk_page(&Config::default());
        // Source selector with a Spotify option.
        assert!(html.contains("<select") && html.contains(r#"value="spotify""#));
        // Posts to the search/extract pipeline and links to Spotify setup.
        assert!(html.contains(r#"hx-post="/search""#));
        assert!(html.contains(r#"href="/spotify""#));
        // Not connected by default -> prompts setup.
        assert!(html.contains("Set up Spotify"));
    }

    #[test]
    fn spotify_page_shows_setup_steps_and_form() {
        let html = render_spotify_page(&Config::default(), None);
        assert!(html.contains("developer.spotify.com/dashboard"), "links to the dashboard");
        assert!(html.contains("Create app"), "walks through creating an app");
        assert!(html.contains(r#"name="spotify_client_id""#));
        assert!(html.contains(r#"hx-post="/spotify""#));
        assert!(html.contains("Not connected"));
    }

    #[test]
    fn spotify_page_reflects_connected_state_without_leaking_the_secret() {
        let mut config = Config::default();
        config.spotify.client_id = Some("pub-id".into());
        config.spotify.client_secret = Some("the-secret".into());
        assert!(spotify_connected(&config));
        let html = render_spotify_page(&config, None);
        assert!(html.contains("Connected"));
        assert!(html.contains("pub-id"), "the public client id is shown");
        assert!(!html.contains("the-secret"), "the secret is never rendered");
    }
}
