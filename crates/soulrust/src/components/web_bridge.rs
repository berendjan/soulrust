//! The HTTP edge: tiny_http worker threads serve the htmx UI by translating
//! requests into bus messages and waiting on per-request reply channels.
//!
//! Each request allocates a correlation id, registers an `mpsc::Sender` in
//! the shared pending map, sends the request message onto the bus, and
//! blocks (with a timeout) for the reply. The `WebBridge` component's
//! handlers complete those channels when the response messages come back.

use std::collections::HashMap;
use std::path::PathBuf;
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
    CancelDownload, ConfigSnapshot, ExtractRequest, ExtractResult, GetConfigReq, HandlerId, PauseDownload,
    HttpHtml, HttpRender, Page, RemoveSearch, SetConfigReq, SetConfigResult, StartDownload, StartSearch,
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
    /// The config file path (for the "View config" fragment); the same path
    /// `main` loaded from.
    config_path: PathBuf,
    /// The pending OAuth `state` nonce between `/spotify/login` and its callback,
    /// for CSRF protection. Shared across HTTP workers.
    oauth_state: Arc<Mutex<Option<String>>>,
    /// Open the UI in the OS default browser once the server is listening.
    open_browser: bool,
}

impl WebBridge {
    pub fn new<W: traits::core::Writer>(ctx: &AppContext, _writer: &W) -> Self {
        WebBridge {
            pending: Arc::new(Mutex::new(HashMap::new())),
            corr: Arc::new(AtomicU64::new(0)),
            bind_addr: ctx.config.ui.bind_addr.clone(),
            control: ctx.control.clone(),
            config_path: ctx.config_path.clone(),
            oauth_state: Arc::new(Mutex::new(None)),
            open_browser: ctx.config.ui.open_browser,
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

        // The server is bound now, so the page is reachable the moment the
        // browser opens. Best-effort: a missing opener (headless box) is ignored.
        if self.open_browser {
            os_open(&format!("http://{}", self.bind_addr));
        }

        for n in 0..HTTP_WORKERS {
            let server = server.clone();
            let shared = SharedBridge {
                pending: self.pending.clone(),
                corr: self.corr.clone(),
                control: self.control.clone(),
                config_path: self.config_path.clone(),
                oauth_state: self.oauth_state.clone(),
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
        let result = match &message.error {
            Some(e) => Err(e.clone()),
            None => Ok(crate::extract::job_from_proto(&message.job)),
        };
        self.complete(message.corr, BridgeReply::Extract(result));
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
        self.complete(message.corr, BridgeReply::Config(Box::new(crate::config::config_from_proto(&message.config))));
    }
}

impl traits::core::Handle<SetConfigResult> for WebBridge {
    fn handle<W: traits::core::Writer>(&mut self, message: &SetConfigResult, _writer: &W) {
        self.complete(message.corr, BridgeReply::SetConfig(message.error.clone().map_or(Ok(()), Err)));
    }
}

impl traits::core::Handle<ApplyUpdateResult> for WebBridge {
    fn handle<W: traits::core::Writer>(&mut self, message: &ApplyUpdateResult, _writer: &W) {
        self.complete(message.corr, BridgeReply::Apply(message.error.clone().map_or(Ok(()), Err)));
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
    config_path: PathBuf,
    oauth_state: Arc<Mutex<Option<String>>>,
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
            WebBridge::send(&HttpRender { corr, page: crate::messages::MessageField::some(crate::messages::page_to_proto(&page)), ..Default::default() }, &self.writer);
        })? {
            BridgeReply::Html(html) => Ok(html),
            _ => Err("unexpected reply type".into()),
        }
    }

    fn handle_request(&self, mut request: tiny_http::Request) {
        let method = request.method().clone();
        let url = request.url().to_string();
        let path = url.split('?').next().unwrap_or("/").to_string();

        // OAuth endpoints respond with redirects, so they bypass the HTML tuple
        // flow below and write their own responses.
        if method.as_str() == "GET" && path == "/spotify/login" {
            self.spotify_login(request);
            return;
        }
        if method.as_str() == "GET" && path == "/spotify/callback" {
            self.spotify_callback(request, &url);
            return;
        }
        // Media streaming needs Range/206 support (custom headers), so it builds
        // its own response rather than going through the HTML tuple flow.
        if method.as_str() == "GET" && path == "/media" {
            self.serve_media(request, &url);
            return;
        }

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
                ("GET", "/fragments/spotify-status") => {
                    self.html_page(self.spotify_status_fragment())
                }
                ("GET", "/fragments/account-status") => {
                    self.html_page(self.render(Page::AccountStatus))
                }
                ("GET", "/account") => self.html_page(self.account_page(None)),
                ("GET", "/downloads") => self.html_page(self.render(Page::Downloads)),
                ("GET", "/fragments/downloads") => {
                    self.html_page(self.render(Page::DownloadsFragment))
                }
                ("GET", "/uploads") => self.html_page(self.render(Page::Uploads)),
                ("GET", "/fragments/uploads") => {
                    self.html_page(self.render(Page::UploadsFragment))
                }
                ("GET", "/bulk") => self.html_page(self.bulk_page()),
                ("GET", "/spotify") => self.html_page(self.spotify_page(None)),
                ("GET", "/config") => self.html_page(self.config_page()),
                ("GET", "/config/view") => self.html_page(self.config_view()),
                ("POST", "/account") => self.html_page(self.save_account(&body)),
                ("POST", "/download") => self.html_page(self.submit_download(&body)),
                ("POST", "/download/cancel") => self.html_page(self.cancel_download(&body)),
                ("POST", "/download/pause") => self.html_page(self.pause_download(&body)),
                ("POST", "/search") => self.html_page(self.submit_search(&body)),
                ("POST", "/search/close") => self.html_page(self.close_search(&body)),
                ("POST", "/open") => self.html_page(self.open_path(&body)),
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
        // "Search again" sends the token of the card to replace; we drop it once
        // the new search has started so the refined one takes its place.
        let replace_token = form
            .get("replace_token")
            .and_then(|v| v.trim().parse::<u32>().ok())
            .filter(|t| *t > 0);

        let extract = match self.round_trip(|corr| {
            WebBridge::send(&ExtractRequest { corr, input: input.clone(), ..Default::default() }, &self.writer);
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

        // "Organize" (bulk playlists): when checked and the source is a playlist
        // or album, downloads land in a subfolder named after it, each track
        // prefixed with a zero-padded index so the folder sorts in track order.
        let organize = form
            .get("organize")
            .is_some_and(|v| v == "1" || v == "on" || v == "true");
        let subdir = organize
            .then(|| job.folder.as_deref().map(crate::components::sanitize_path_component))
            .flatten()
            .filter(|s| !s.is_empty());
        // Pad to the width of the largest index, with a floor of 2 digits
        // (01, 02, …; 001, 002, … once a playlist has 100+ tracks).
        let width = job.searches.len().to_string().len().max(2);
        let jobs: Vec<_> = job
            .searches
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let mut proto = crate::extract::searchjob_to_proto(s);
                if let Some(folder) = &subdir {
                    proto.folder = folder.clone();
                    proto.prefix = format!("{n:0width$} ", n = i + 1, width = width);
                }
                proto
            })
            .collect();

        let search = match self.round_trip(|corr| {
            WebBridge::send(
                &StartSearch {
                    corr,
                    source_label: job.source_label.clone(),
                    jobs: jobs.clone(),
                    ..Default::default()
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
            (started, None) => {
                if let Some(token) = replace_token {
                    WebBridge::send(&RemoveSearch { token, ..Default::default() }, &self.writer);
                }
                format!(
                    r#"<div class="banner">{} — started {} search(es)</div>"#,
                    escape(&job.source_label),
                    started.len()
                )
            }
        };
        let fragment = self.render(Page::SearchesFragment)?;
        Ok(format!("{banner}{fragment}"))
    }

    /// POST /search/close: drop a search card from the UI state.
    fn close_search(&self, body: &str) -> Result<String, String> {
        if let Some(token) = parse_form(body).get("token").and_then(|v| v.trim().parse::<u32>().ok()) {
            WebBridge::send(&RemoveSearch { token, ..Default::default() }, &self.writer);
        }
        Ok(String::new())
    }

    /// POST /open: open a local directory in the OS file manager. Only existing
    /// directories are opened (benign), so a stray request can't launch files.
    fn open_path(&self, body: &str) -> Result<String, String> {
        if let Some(path) = parse_form(body).get("path") {
            if !path.is_empty() && std::path::Path::new(path).is_dir() {
                os_open(path);
            }
        }
        Ok(String::new())
    }

    /// GET /media?path=…: stream a finished audio file for the in-browser mini
    /// player, honoring `Range` requests so the browser shows a seekable
    /// timeline (without Range it renders a non-seekable "live" stream).
    /// Restricted to existing files with a known audio extension so a stray
    /// request can't read arbitrary files off disk.
    fn serve_media(&self, request: tiny_http::Request, url: &str) {
        let fail = |request: tiny_http::Request, code: u32, msg: &str| {
            let _ = request.respond(tiny_http::Response::from_string(msg).with_status_code(code));
        };
        let path = match parse_form(url.split('?').nth(1).unwrap_or("")).get("path") {
            Some(p) if !p.is_empty() => p.clone(),
            _ => return fail(request, 400, "missing path"),
        };
        let p = std::path::Path::new(&path);
        let mime = match p.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
            Some("mp3") => "audio/mpeg",
            Some("flac") => "audio/flac",
            Some("wav") => "audio/wav",
            Some("m4a") | Some("m4b") | Some("aac") | Some("mp4") => "audio/mp4",
            Some("ogg") | Some("opus") => "audio/ogg",
            Some("aiff") | Some("aif") => "audio/aiff",
            _ => return fail(request, 415, "unsupported media type"),
        };
        if !p.is_file() {
            return fail(request, 404, "not found");
        }
        let bytes = match std::fs::read(p) {
            Ok(b) => b,
            Err(_) => return fail(request, 500, "read error"),
        };
        let total = bytes.len();

        let range = request
            .headers()
            .iter()
            .find(|h| h.field.equiv("Range"))
            .and_then(|h| parse_byte_range(h.value.as_str(), total));

        let (status, start, end) = match range {
            Some((s, e)) => (206u32, s, e),
            None => (200u32, 0, total.saturating_sub(1)),
        };
        let body = if total == 0 { Vec::new() } else { bytes[start..=end].to_vec() };

        let hdr = |k: &str, v: &str| tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap();
        let mut response = tiny_http::Response::from_data(body)
            .with_status_code(status)
            .with_header(hdr("Content-Type", mime))
            .with_header(hdr("Accept-Ranges", "bytes"));
        if status == 206 {
            response = response.with_header(hdr("Content-Range", &format!("bytes {start}-{end}/{total}")));
        }
        let _ = request.respond(response);
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
        // "Organize" destination hints carried by the result row's Get form.
        let subdir = form.get("subdir").cloned().unwrap_or_default();
        let prefix = form.get("prefix").cloned().unwrap_or_default();
        if username.is_empty() || filename.is_empty() {
            return Ok(r#"<span class="pill warn">bad request</span>"#.into());
        }
        WebBridge::send(
            &StartDownload { username, filename, size, subdir, prefix, ..Default::default() },
            &self.writer,
        );
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
            &CancelDownload { username: username.clone(), filename: filename.clone(), ..Default::default() },
            &self.writer,
        );
        Ok(format!(
            r##"<form hx-post="/download" hx-target="this" hx-swap="outerHTML" style="margin:0"><input type="hidden" name="username" value="{user}"><input type="hidden" name="filename" value="{path}"><input type="hidden" name="size" value="{size}"><button class="btn xs" type="submit">Get</button></form>"##,
            user = escape(&username),
            path = escape(&filename),
            size = size,
        ))
    }

    /// POST /download/pause: pause an active download (abort but keep the
    /// partial; the UI moves the row to a Paused state with a Resume button on
    /// the next poll). The active row uses hx-swap="delete", so the body is
    /// ignored.
    fn pause_download(&self, body: &str) -> Result<String, String> {
        let form = parse_form(body);
        let username = form.get("username").cloned().unwrap_or_default();
        let filename = form.get("filename").cloned().unwrap_or_default();
        WebBridge::send(&PauseDownload { username, filename, ..Default::default() }, &self.writer);
        Ok(String::new())
    }

    /// GET /fragments/browse: render the current browse state (owned by the
    /// Browse read-model component).
    fn browse_fragment(&self) -> Result<String, String> {
        match self.round_trip(|corr| {
            WebBridge::send(&BrowseRenderReq { corr, ..Default::default() }, &self.writer);
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
            WebBridge::send(&BrowseUser { corr, username: username.clone(), ..Default::default() }, &self.writer);
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
            WebBridge::send(&GetConfigReq { corr, ..Default::default() }, &self.writer);
        })? {
            BridgeReply::Config(config) => Ok(*config),
            _ => Err("unexpected reply type".into()),
        }
    }

    fn config_page(&self) -> Result<String, String> {
        let config = self.current_config()?;
        Ok(render_config_page(&config, None))
    }

    /// GET /config/view: the effective configuration serialized to YAML (with
    /// secrets redacted), plus the on-disk path and whether the file exists yet.
    fn config_view(&self) -> Result<String, String> {
        let config = self.current_config()?;
        let path = self.config_path.display().to_string();
        let (pill, note) = if self.config_path.exists() {
            (r#"<span class="pill ok">on disk</span>"#, "")
        } else {
            (
                r#"<span class="pill warn">not saved yet</span>"#,
                " — showing the effective defaults; the file is written on the first Save.",
            )
        };
        Ok(format!(
            r#"<div class="card"><h3 style="margin-top:0">Effective configuration</h3><p class="muted" style="margin-top:0"><code>{path}</code> {pill}{note}</p><pre class="log">{yaml}</pre></div>"#,
            path = escape(&path),
            pill = pill,
            note = escape(note),
            yaml = escape(&config_as_redacted_yaml(&config)),
        ))
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
            WebBridge::send(&SetConfigReq { corr, config: soulrust_proto::MessageField::some(crate::config::config_to_proto(&config)), ..Default::default() }, &self.writer);
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
            WebBridge::send(&ApplyUpdateReq { corr, ..Default::default() }, &self.writer);
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
            WebBridge::send(&SetConfigReq { corr, config: soulrust_proto::MessageField::some(crate::config::config_to_proto(&config)), ..Default::default() }, &self.writer);
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

    /// GET /fragments/spotify-status: the live connection hint under the bulk
    /// input on the index page. Green pill once logged in, otherwise the amber
    /// "needs Spotify connected" prompt. Polled by htmx so it flips the moment
    /// the OAuth login completes, no page reload.
    fn spotify_status_fragment(&self) -> Result<String, String> {
        let config = self.current_config()?;
        Ok(render_spotify_status_hint(&config))
    }

    /// GET /spotify: how to connect Spotify (create an app, get keys) plus the
    /// credential form. `banner` is shown after a save.
    fn spotify_page(&self, banner: Option<String>) -> Result<String, String> {
        let config = self.current_config()?;
        Ok(render_spotify_page(&config, banner, None))
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
            WebBridge::send(&SetConfigReq { corr, config: soulrust_proto::MessageField::some(crate::config::config_to_proto(&config)), ..Default::default() }, &self.writer);
        })? {
            BridgeReply::SetConfig(result) => result,
            _ => return Err("unexpected reply type".into()),
        };

        // Save failed at the config layer: nothing to verify, surface the error.
        if let Err(err) = &result {
            let banner = format!(r#"<div class="banner error">{}</div>"#, escape(err));
            return Ok(render_spotify_page(&config, Some(banner), None));
        }

        // Saved. Immediately exercise the client-credentials flow so the status
        // reflects whether Spotify actually accepts the keys, not just that
        // they're non-empty. If either field is blank we skip the network call.
        let (banner, verified) = match (
            config.spotify.client_id.as_deref(),
            config.spotify.client_secret.as_deref(),
        ) {
            (Some(id), Some(secret)) if !id.is_empty() && !secret.is_empty() => {
                match verify_spotify_credentials(id, secret) {
                    Ok(()) => (
                        r#"<div class="banner">Spotify credentials saved and verified — now click <strong>Log in with Spotify</strong> below.</div>"#.to_string(),
                        Some(true),
                    ),
                    Err(err) => (
                        format!(
                            r#"<div class="banner error">Credentials saved, but Spotify rejected them: {}</div>"#,
                            escape(&err)
                        ),
                        Some(false),
                    ),
                }
            }
            _ => (
                r#"<div class="banner">Spotify credentials saved.</div>"#.to_string(),
                None,
            ),
        };
        Ok(render_spotify_page(&config, Some(banner), verified))
    }

    /// The redirect URI registered with Spotify — derived from the UI bind
    /// address so it matches exactly what the user entered in the dashboard.
    fn spotify_redirect_uri(&self, config: &Config) -> String {
        format!("http://{}/spotify/callback", config.ui.bind_addr)
    }

    /// Mint a fresh OAuth `state` nonce and remember it for the callback to
    /// verify. Not cryptographic, but enough to bind a callback to a login we
    /// started on this loopback-only server.
    fn new_oauth_state(&self) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seq = self.corr.fetch_add(1, Ordering::Relaxed);
        let state = format!("{nanos:x}{seq:x}");
        *self.oauth_state.lock().unwrap() = Some(state.clone());
        state
    }

    /// Build the Spotify authorize URL to redirect the browser to.
    fn spotify_authorize_url(&self) -> Result<String, String> {
        let config = self.current_config()?;
        let client_id = config
            .spotify
            .client_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or("set your Spotify Client ID first (see the Connect Spotify page)")?;
        let redirect_uri = self.spotify_redirect_uri(&config);
        let state = self.new_oauth_state();
        Ok(format!(
            "https://accounts.spotify.com/authorize?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}",
            percent_encode(client_id),
            percent_encode(&redirect_uri),
            percent_encode("playlist-read-private playlist-read-collaborative"),
            percent_encode(&state),
        ))
    }

    /// GET /spotify/login: 302 the browser to Spotify's authorize screen.
    fn spotify_login(&self, request: tiny_http::Request) {
        match self.spotify_authorize_url() {
            Ok(location) => {
                let header = tiny_http::Header::from_bytes(&b"Location"[..], location.as_bytes())
                    .expect("location header");
                let _ = request.respond(tiny_http::Response::empty(302).with_header(header));
            }
            Err(err) => {
                let html = format!(r#"<div class="banner error">{}</div>"#, escape(&err));
                let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/html"[..])
                    .expect("content-type header");
                let _ = request.respond(
                    tiny_http::Response::from_data(html.into_bytes())
                        .with_status_code(400)
                        .with_header(header),
                );
            }
        }
    }

    /// GET /spotify/callback: verify state, exchange the code for a refresh
    /// token, persist it, then bounce back to the Connect Spotify page.
    fn spotify_callback(&self, request: tiny_http::Request, url: &str) {
        match self.complete_spotify_login(url) {
            Ok(()) => {
                let header = tiny_http::Header::from_bytes(&b"Location"[..], &b"/spotify"[..])
                    .expect("location header");
                let _ = request.respond(tiny_http::Response::empty(302).with_header(header));
            }
            Err(err) => {
                let banner =
                    format!(r#"<div class="banner error">Spotify login failed: {}</div>"#, escape(&err));
                let html = self
                    .spotify_page(Some(banner))
                    .unwrap_or_else(|e| format!("<p>error: {}</p>", escape(&e)));
                let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/html"[..])
                    .expect("content-type header");
                let _ = request.respond(
                    tiny_http::Response::from_data(html.into_bytes())
                        .with_status_code(400)
                        .with_header(header),
                );
            }
        }
    }

    fn complete_spotify_login(&self, url: &str) -> Result<(), String> {
        let params = parse_form(url.split('?').nth(1).unwrap_or(""));
        if let Some(err) = params.get("error") {
            return Err(format!("Spotify returned '{err}'"));
        }
        let code = params
            .get("code")
            .filter(|s| !s.is_empty())
            .ok_or("callback had no authorization code")?;
        let returned_state = params.get("state").cloned().unwrap_or_default();
        // Consume the pending state and require an exact match.
        match self.oauth_state.lock().unwrap().take() {
            Some(expected) if expected == returned_state => {}
            _ => return Err("state mismatch — please start the login again".into()),
        }

        let mut config = self.current_config()?;
        let client_id = config
            .spotify
            .client_id
            .clone()
            .filter(|s| !s.is_empty())
            .ok_or("Spotify Client ID is not set")?;
        let client_secret = config
            .spotify
            .client_secret
            .clone()
            .filter(|s| !s.is_empty())
            .ok_or("Spotify Client Secret is not set")?;
        let redirect_uri = self.spotify_redirect_uri(&config);

        let tokens = crate::extract::spotify::exchange_authorization_code(
            &client_id,
            &client_secret,
            code,
            &redirect_uri,
        )?;
        config.spotify.refresh_token = Some(tokens.refresh_token);

        // Persist via the config store, which also broadcasts ConfigChanged so
        // the extractor picks up the new refresh token.
        match self.round_trip(|corr| {
            WebBridge::send(
                &SetConfigReq {
                    corr,
                    config: soulrust_proto::MessageField::some(crate::config::config_to_proto(&config)),
                    ..Default::default()
                },
                &self.writer,
            );
        })? {
            BridgeReply::SetConfig(result) => result,
            _ => Err("unexpected reply type".into()),
        }
    }
}

/// Exercise the Spotify client-credentials flow to confirm the keys work.
/// Returns `Ok(())` when Spotify issues a token, `Err` with the reason
/// otherwise (bad keys, network failure, …).
fn verify_spotify_credentials(client_id: &str, client_secret: &str) -> Result<(), String> {
    // A one-shot client-credentials token request. Done inline rather than via
    // the `SpotifyApi` trait, which now models the OAuth *user-login* flow
    // (refresh token) and no longer exposes an app-only token call. Credentials
    // go in the form body (a supported alternative to the Basic auth header) to
    // avoid a base64 dependency.
    let response: serde_json::Value = ureq::post("https://accounts.spotify.com/api/token")
        .send_form(&[
            ("grant_type", "client_credentials"),
            ("client_id", client_id),
            ("client_secret", client_secret),
        ])
        .map_err(|e| format!("token request failed: {e}"))?
        .into_json()
        .map_err(|e| format!("token response is not json: {e}"))?;
    if response["access_token"].as_str().is_some() {
        Ok(())
    } else {
        Err("token response missing access_token".into())
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

/// True once the user has completed the OAuth login (a refresh token is stored).
fn spotify_logged_in(config: &Config) -> bool {
    config.spotify.refresh_token.as_deref().is_some_and(|s| !s.is_empty())
}

/// True when both app credentials (client id + secret) are present — the
/// prerequisite for starting the login.
fn spotify_credentials_set(config: &Config) -> bool {
    config.spotify.client_id.as_deref().is_some_and(|s| !s.is_empty())
        && config.spotify.client_secret.as_deref().is_some_and(|s| !s.is_empty())
}

/// A status pill + next-step button for Spotify, shown on the bulk + spotify
/// pages: logged in, credentials-set-but-not-logged-in, or nothing set.
fn spotify_status_card(config: &Config) -> String {
    if spotify_logged_in(config) {
        r#"<div class="card"><span class="pill ok">● Spotify connected</span>
<a class="btn secondary" style="margin-left:0.6rem" href="/spotify">Manage</a></div>"#
            .to_string()
    } else if spotify_credentials_set(config) {
        r#"<div class="card"><span class="pill warn">● Spotify not logged in</span>
<a class="btn spotify" style="margin-left:0.6rem" href="/spotify/login">Log in with Spotify</a>
<p class="muted" style="margin:0.6rem 0 0">Log in so soulrust can read your playlists.</p></div>"#
            .to_string()
    } else {
        r#"<div class="card"><span class="pill warn">● Spotify not connected</span>
<a class="btn spotify" style="margin-left:0.6rem" href="/spotify">Set up Spotify</a>
<p class="muted" style="margin:0.6rem 0 0">Connect Spotify to paste playlist, album, and track links.</p></div>"#
            .to_string()
    }
}

/// The compact connection hint shown under the bulk input on the index page.
/// Once logged in it's a green "Spotify connected" pill; otherwise it's the
/// prompt to go connect one.
fn render_spotify_status_hint(config: &Config) -> String {
    if spotify_logged_in(config) {
        r#"<p style="margin:0.5rem 0 0"><span class="pill ok">● Spotify connected</span></p>"#
            .to_string()
    } else {
        r#"<p class="muted" style="margin:0.5rem 0 0">Paste a playlist, album, or track link — needs <a href="/spotify">Spotify connected</a>.</p>"#
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
  <label style="display:flex; align-items:center; gap:0.4rem; font-weight:normal; margin:0 0 0.9rem">
    <input type="checkbox" name="organize" value="1" checked>
    Organize into a folder named after the playlist, numbering tracks (01, 02, …)
  </label>
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

/// `verified` reflects a just-completed credential check after a save:
/// `Some(false)` means Spotify rejected the keys — the pill shows "Not
/// connected" regardless of what's stored. `Some(true)` / `None` (e.g. a plain
/// GET) fall through to the login-state pill (logged in / not logged in).
fn render_spotify_page(config: &Config, banner: Option<String>, verified: Option<bool>) -> String {
    let redirect_uri = format!("http://{}/spotify/callback", config.ui.bind_addr);
    let status = if verified == Some(false) {
        r#"<span class="pill warn">● Not connected</span>"#
    } else if spotify_logged_in(config) {
        r#"<span class="pill ok">● Logged in</span>"#
    } else if spotify_credentials_set(config) {
        r#"<span class="pill warn">● Not logged in</span>"#
    } else {
        r#"<span class="pill warn">● Not connected</span>"#
    };
    // The login button only appears once app credentials are saved.
    let login = if spotify_credentials_set(config) {
        let (heading, label) = if spotify_logged_in(config) {
            ("Logged in", "Re-log in with Spotify")
        } else {
            ("Log in", "Log in with Spotify")
        };
        format!(
            r#"<div class="card"><h2 style="margin-top:0">{heading}</h2>
<p class="muted" style="margin-top:0">soulrust opens Spotify in your browser to authorize read-only access to your playlists. Spotify only serves playlists you own or collaborate on.</p>
<a class="btn spotify" href="/spotify/login">{label}</a></div>"#
        )
    } else {
        String::new()
    };
    let body = format!(
        r#"<h1>Connect Spotify</h1>
<p class="sub">{status} &nbsp; soulrust reads your Spotify playlists (plus albums and tracks) to turn them into searches. Create a free Spotify app once, paste its two keys, then log in.</p>
{banner}
<div class="card">
<h2 style="margin-top:0">Get your keys (about 2 minutes)</h2>
<ol class="steps">
  <li>Open the <a href="https://developer.spotify.com/dashboard" target="_blank" rel="noopener">Spotify Developer Dashboard</a> and log in with any Spotify account (a free account works).</li>
  <li>Click <strong>Create app</strong>.</li>
  <li>Fill in an <strong>App name</strong> (e.g. <code>soulrust</code>) and a short description. For <strong>Redirect URI</strong> enter <code>{redirect_uri}</code> <strong>exactly</strong> — this is where Spotify sends you back after you log in. Under "Which API/SDKs…?" tick <strong>Web API</strong>, accept the terms, and click <strong>Save</strong>.</li>
  <li>Open your new app and go to <strong>Settings</strong>.</li>
  <li>Copy the <strong>Client ID</strong>. Click <strong>View client secret</strong> and copy the <strong>Client secret</strong>.</li>
  <li>Paste both below and click <strong>Save</strong>, then <strong>Log in with Spotify</strong>.</li>
</ol>
<p class="muted">This uses Spotify's Authorization Code flow: after you approve it once, soulrust gets read-only access to your playlists and stores only a refresh token locally — it never sees your password.</p>
</div>
<div class="card">
<form hx-post="/spotify" hx-target="body">
  <label>Client ID <input type="text" name="spotify_client_id" value="{client_id}" placeholder="e.g. 1a2b3c4d5e6f..."></label>
  <label>Client secret <input type="password" name="spotify_client_secret" value="" placeholder="{secret_ph}"></label>
  <p style="margin-top:0.9rem"><button class="btn spotify" type="submit">Save</button>
  <a class="btn secondary" style="margin-left:0.5rem" href="/bulk">Back to bulk downloads</a></p>
</form>
</div>
{login}"#,
        status = status,
        banner = banner.unwrap_or_default(),
        redirect_uri = escape(&redirect_uri),
        client_id = escape(config.spotify.client_id.as_deref().unwrap_or("")),
        secret_ph = secret_placeholder(
            config.spotify.client_secret.as_deref().unwrap_or(""),
            "paste the client secret",
        ),
        login = login,
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
        r##"<h1>Settings</h1>
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
</form>
<div class="card"><h2 style="margin-top:0">Download folder</h2>
<p class="muted" style="margin-top:0">Finished downloads land here. <code>{open_path}</code></p>
<form hx-post="/open" hx-swap="none" style="margin:0"><input type="hidden" name="path" value="{open_path}"><button class="btn secondary" type="submit">Open download folder</button></form>
</div>
<div class="card"><h2 style="margin-top:0">Inspect</h2>
<p class="muted" style="margin-top:0">See the effective configuration as stored YAML (the password and Spotify secret are hidden).</p>
<button class="btn secondary" hx-get="/config/view" hx-target="#config-view" hx-swap="innerHTML">View config</button>
<div id="config-view" style="margin-top:1rem"></div>
</div>"##,
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
        open_path = escape(&config.sharing.download_path().display().to_string()),
    );
    shell("soulrust — settings", "config", &config.server.username, &body)
}

/// Serialize a config to YAML for display, with secret fields masked so the
/// view never reveals the Soulseek password or Spotify client secret (the rest
/// of the settings UI is careful never to render them either).
fn config_as_redacted_yaml(config: &Config) -> String {
    const HIDDEN: &str = "(hidden)";
    let mut c = config.clone();
    if !c.server.password.is_empty() {
        c.server.password = HIDDEN.into();
    }
    if c.spotify.client_secret.as_deref().is_some_and(|s| !s.is_empty()) {
        c.spotify.client_secret = Some(HIDDEN.into());
    }
    serde_yaml::to_string(&c).unwrap_or_else(|e| format!("failed to serialize config: {e}"))
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

/// Parse a single-range HTTP `Range` header (`bytes=start-end`, with either end
/// optional, or a `bytes=-suffix`) into an inclusive `(start, end)` within a file
/// of `total` bytes. Returns `None` for absent/unsatisfiable/multi-range values,
/// in which case the caller serves the whole file (200).
fn parse_byte_range(header: &str, total: usize) -> Option<(usize, usize)> {
    if total == 0 {
        return None;
    }
    let spec = header.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None; // multi-range not supported
    }
    let (start_s, end_s) = spec.split_once('-')?;
    let (start, end) = if start_s.is_empty() {
        // Suffix range: the last N bytes.
        let n: usize = end_s.trim().parse().ok()?;
        if n == 0 {
            return None;
        }
        (total.saturating_sub(n), total - 1)
    } else {
        let start: usize = start_s.trim().parse().ok()?;
        if start >= total {
            return None;
        }
        let end = if end_s.trim().is_empty() {
            total - 1
        } else {
            end_s.trim().parse::<usize>().ok()?.min(total - 1)
        };
        (start, end)
    };
    if end < start {
        return None;
    }
    Some((start, end))
}

/// Open `target` (a URL or a local directory) with the OS default handler,
/// detached. Best-effort: any failure (no opener on a headless box, spawn error)
/// is ignored so it never breaks the caller.
fn os_open(target: &str) {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut c = std::process::Command::new("open");
        c.arg(target);
        c
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        // `start` is a cmd builtin; the empty "" is its window-title argument.
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", target]);
        c
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut command = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(target);
        c
    };
    let _ = command.spawn();
}

/// Percent-encode a query value: keep the RFC 3986 unreserved set, escape the
/// rest (so a redirect URI's `:` and `/` and the scope's spaces are encoded).
fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
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
        assert!(html.contains(r#"href="/""#) && html.contains(r#"href="/spotify""#));
    }

    #[test]
    fn config_page_has_a_view_config_control() {
        let html = render_config_page(&Config::default(), None);
        assert!(html.contains(r#"hx-get="/config/view""#), "settings page exposes View config");
        assert!(html.contains("View config"));
        assert!(html.contains(r#"id="config-view""#), "has a target container for the fragment");
    }

    #[test]
    fn redacted_yaml_hides_secrets_but_keeps_other_fields() {
        let mut config = Config::default();
        config.server.username = "alice".into();
        config.server.password = "hunter2".into();
        config.spotify.client_id = Some("pub-id".into());
        config.spotify.client_secret = Some("sssh".into());
        let yaml = config_as_redacted_yaml(&config);
        assert!(!yaml.contains("hunter2"), "password is masked");
        assert!(!yaml.contains("sssh"), "client secret is masked");
        assert!(yaml.contains("alice"), "non-secret fields are shown");
        assert!(yaml.contains("pub-id"), "the public client id is shown");
        assert!(yaml.contains("(hidden)"));
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
        let html = render_spotify_page(&Config::default(), None, None);
        assert!(html.contains("developer.spotify.com/dashboard"), "links to the dashboard");
        assert!(html.contains("Create app"), "walks through creating an app");
        assert!(html.contains(r#"name="spotify_client_id""#));
        assert!(html.contains(r#"hx-post="/spotify""#));
        assert!(html.contains("Not connected"));
    }

    #[test]
    fn spotify_page_offers_login_once_credentials_are_set() {
        let mut config = Config::default();
        config.spotify.client_id = Some("pub-id".into());
        config.spotify.client_secret = Some("the-secret".into());
        assert!(spotify_credentials_set(&config) && !spotify_logged_in(&config));
        let html = render_spotify_page(&config, None, None);
        // Credentials set but not logged in -> prompts login.
        assert!(html.contains("Not logged in"));
        assert!(html.contains(r#"href="/spotify/login""#), "shows the login button");
        assert!(html.contains("pub-id"), "the public client id is shown");
        assert!(!html.contains("the-secret"), "the secret is never rendered");
    }

    #[test]
    fn spotify_page_shows_logged_in_and_the_callback_redirect_uri() {
        let mut config = Config::default();
        config.spotify.client_id = Some("pub-id".into());
        config.spotify.client_secret = Some("the-secret".into());
        config.spotify.refresh_token = Some("rt".into());
        let html = render_spotify_page(&config, None, None);
        assert!(spotify_logged_in(&config) && html.contains("Logged in"));
        // The exact redirect URI the user must register is shown.
        assert!(html.contains("http://127.0.0.1:5030/spotify/callback"));
    }

    #[test]
    fn spotify_page_verified_override_beats_credential_presence() {
        let mut config = Config::default();
        config.spotify.client_id = Some("pub-id".into());
        config.spotify.client_secret = Some("the-secret".into());
        // Credentials are present, but Spotify rejected them on save: the pill
        // must show "Not connected" rather than trusting mere presence.
        let html = render_spotify_page(&config, None, Some(false));
        assert!(html.contains("Not connected"));
    }

    #[test]
    fn byte_range_parsing() {
        assert_eq!(parse_byte_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_byte_range("bytes=100-", 1000), Some((100, 999)));
        assert_eq!(parse_byte_range("bytes=-200", 1000), Some((800, 999)));
        assert_eq!(parse_byte_range("bytes=0-99999", 1000), Some((0, 999)), "end clamps to EOF");
        assert_eq!(parse_byte_range("bytes=2000-", 1000), None, "start past EOF");
        assert_eq!(parse_byte_range("bytes=0-10,20-30", 1000), None, "multi-range unsupported");
        assert_eq!(parse_byte_range("bytes=0-99", 0), None, "empty file");
        assert_eq!(parse_byte_range("nonsense", 1000), None);
    }

    #[test]
    fn percent_encode_escapes_reserved_characters() {
        assert_eq!(
            percent_encode("http://127.0.0.1:5030/spotify/callback"),
            "http%3A%2F%2F127.0.0.1%3A5030%2Fspotify%2Fcallback"
        );
        assert_eq!(percent_encode("a b-c_d.e~f"), "a%20b-c_d.e~f");
    }
}
