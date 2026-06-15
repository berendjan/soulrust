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

use crate::config::{AppContext, Config, Control};
use crate::extract::Job;
use crate::messages::{
    ApplyUpdateReq, ApplyUpdateResult, BrowseAccepted, BrowseHtml, BrowseRenderReq, BrowseUser,
    ConfigSnapshot, ExtractRequest, ExtractResult, GetConfigReq, HandlerId, HttpHtml, HttpRender,
    Page, SetConfigReq, SetConfigResult, StartSearch, StartSearchResult, StartedSearch,
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
                ("GET", "/") => self.html_page(self.render(Page::Index)),
                ("GET", "/fragments/status") => self.html_page(self.render(Page::StatusFragment)),
                ("GET", "/fragments/searches") => {
                    self.html_page(self.render(Page::SearchesFragment))
                }
                ("GET", "/fragments/browse") => self.html_page(self.browse_fragment()),
                ("GET", "/config") => self.html_page(self.config_page()),
                ("POST", "/search") => self.html_page(self.submit_search(&body)),
                ("POST", "/browse") => self.html_page(self.submit_browse(&body)),
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
        config.update.enabled = form.contains_key("update_enabled");
        config.update.auto_apply = form.contains_key("update_auto_apply");
        if let Some(v) = get("update_repo") {
            config.update.repo = v;
        }
        if let Some(v) = get("bind_addr") {
            config.ui.bind_addr = v;
        }

        let result = match self.round_trip(|corr| {
            WebBridge::send(&SetConfigReq { corr, config: config.clone() }, &self.writer);
        })? {
            BridgeReply::SetConfig(result) => result,
            _ => return Err("unexpected reply type".into()),
        };

        let banner = match &result {
            Ok(()) => r#"<div class="banner">configuration saved — server/spotify changes apply after a restart</div>"#.to_string(),
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
}

fn render_config_page(config: &Config, banner: Option<String>) -> String {
    let checked = |b: bool| if b { "checked" } else { "" };
    format!(
        r#"<!DOCTYPE html>
<html>
<head><title>soulrust configuration</title><script src="/assets/htmx.min.js"></script>
<style>
body {{ font-family: sans-serif; max-width: 36rem; margin: 2rem auto; padding: 0 1rem; }}
label {{ display: block; margin-top: 0.8rem; }}
input[type=text], input[type=password] {{ width: 100%; padding: 0.4rem; box-sizing: border-box; }}
.banner {{ padding: 0.5rem; border-radius: 4px; background: #eef; margin: 0.5rem 0; }}
.error {{ background: #fee; }}
fieldset {{ margin-top: 1rem; }}
</style></head>
<body>
<h1>configuration</h1>
<div id="result">{banner}</div>
<form hx-post="/config" hx-target="body">
<fieldset><legend>soulseek server</legend>
<label>host <input type="text" name="host" value="{host}"></label>
<label>port <input type="text" name="port" value="{port}"></label>
<label>username <input type="text" name="username" value="{username}"></label>
<label>password (leave empty to keep) <input type="password" name="password" value=""></label>
<label>listen port <input type="text" name="listen_port" value="{listen_port}"></label>
</fieldset>
<fieldset><legend>spotify</legend>
<label>client id <input type="text" name="spotify_client_id" value="{client_id}"></label>
<label>client secret (leave empty to keep) <input type="password" name="spotify_client_secret" value=""></label>
</fieldset>
<fieldset><legend>updates</legend>
<label><input type="checkbox" name="update_enabled" {enabled}> check for updates on startup</label>
<label><input type="checkbox" name="update_auto_apply" {auto_apply}> apply automatically</label>
<label>github repo <input type="text" name="update_repo" value="{repo}"></label>
</fieldset>
<fieldset><legend>ui</legend>
<label>bind address <input type="text" name="bind_addr" value="{bind_addr}"></label>
</fieldset>
<p><button type="submit">save</button> <a href="/">back</a></p>
</form>
</body></html>"#,
        banner = banner.unwrap_or_default(),
        host = escape(&config.server.host),
        port = config.server.port,
        username = escape(&config.server.username),
        listen_port = config.server.listen_port,
        client_id = escape(config.spotify.client_id.as_deref().unwrap_or("")),
        enabled = checked(config.update.enabled),
        auto_apply = checked(config.update.auto_apply),
        repo = escape(&config.update.repo),
        bind_addr = escape(&config.ui.bind_addr),
    )
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
    }
}
