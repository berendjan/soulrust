//! The UI component: holds the view state and renders htmx pages/fragments.
//! It is the single consumer of all view-relevant events; the web bridge
//! turns HTTP requests into [`HttpRender`] messages and serves whatever HTML
//! comes back.

use std::collections::VecDeque;

use rust_messenger::traits;
use rust_messenger::traits::extended::Sender;

use crate::config::AppContext;
use crate::messages::{
    ConfigChanged, DownloadComplete, DownloadFailed, DownloadQueuePosition, HandlerId, HttpHtml,
    HttpRender, Page, PeerActivity, SearchResultReceived, SessionEvent, SessionEventKind,
    UpdaterStatus, UpdaterStatusChanged, UploadComplete, UploadFailed,
};

const MAX_LOG_LINES: usize = 100;
/// Cap on results kept per search, so a flood of responses can't grow the UI
/// state without bound (filtering already drops the worst before they arrive).
const MAX_RESULTS_PER_SEARCH: usize = 200;

#[derive(Debug, Clone, PartialEq)]
enum SessionStatus {
    Disconnected(String),
    Connecting,
    LoggedIn { greeting: String, own_ip: String },
    LoginFailed(String),
}

/// One peer's filter-passing response to a search.
struct SearchResultRow {
    username: String,
    free_slots: bool,
    upload_speed: u32,
    in_queue: u32,
    files: Vec<(String, u64)>,
}

struct SearchRow {
    token: u32,
    query: String,
    results: Vec<SearchResultRow>,
}

pub struct Ui {
    session: SessionStatus,
    searches: Vec<SearchRow>,
    updater: Option<UpdaterStatus>,
    log: VecDeque<String>,
    username: String,
}

impl Ui {
    pub fn new<W: traits::core::Writer>(ctx: &AppContext, _writer: &W) -> Self {
        Ui {
            session: SessionStatus::Disconnected("starting up".into()),
            searches: Vec::new(),
            updater: None,
            log: VecDeque::new(),
            username: ctx.config.server.username.clone(),
        }
    }

    fn log(&mut self, line: String) {
        if self.log.len() >= MAX_LOG_LINES {
            self.log.pop_front();
        }
        self.log.push_back(line);
    }

    fn render(&self, page: &Page) -> String {
        match page {
            Page::Index => self.render_index(),
            Page::StatusFragment => self.render_status(),
            Page::SearchesFragment => self.render_searches(),
            Page::ConfigForm => self.render_config_note(),
        }
    }

    fn render_index(&self) -> String {
        let body = r##"<h1>Search</h1>
<p class="sub">Search the Soulseek network or browse a user's shared files. For many tracks at once, use <a href="/bulk">Bulk downloads</a>.</p>
<div id="status" hx-get="/fragments/status" hx-trigger="load, every 2s"></div>
<div class="card">
<form hx-post="/search" hx-target="#searches" hx-swap="innerHTML" style="display:flex; gap:0.5rem; align-items:flex-end;">
  <div style="flex:1"><label for="q" style="margin-top:0">Search</label>
  <input id="q" type="text" name="input" placeholder="search text, or a spotify playlist / album / track link" autofocus></div>
  <button class="btn" type="submit">Search</button>
</form>
</div>
<div id="searches" hx-get="/fragments/searches" hx-trigger="load, every 2s"></div>
<h2>Browse a user's shares</h2>
<div class="card">
<form hx-post="/browse" hx-target="#browse" hx-swap="innerHTML" style="display:flex; gap:0.5rem; align-items:flex-end;">
  <div style="flex:1"><label for="u" style="margin-top:0">Username</label>
  <input id="u" type="text" name="username" placeholder="soulseek username"></div>
  <button class="btn secondary" type="submit">Browse</button>
</form>
</div>
<div id="browse" hx-get="/fragments/browse" hx-trigger="load, every 3s"></div>"##;
        crate::components::ui_theme::shell("soulrust", "search", body)
    }

    fn render_status(&self) -> String {
        let session = match &self.session {
            SessionStatus::Disconnected(reason) => {
                format!(
                    r#"<span>disconnected ({})</span>"#,
                    escape(reason)
                )
            }
            SessionStatus::Connecting => "<span>connecting…</span>".into(),
            SessionStatus::LoggedIn { greeting, own_ip } => format!(
                r#"<span>logged in as <b>{}</b> ({}) — {}</span>"#,
                escape(&self.username),
                escape(own_ip),
                escape(greeting)
            ),
            SessionStatus::LoginFailed(reason) => format!(
                r#"<span class="error">login failed: {}</span>"#,
                escape(reason)
            ),
        };

        let updater = match &self.updater {
            None => String::new(),
            Some(status) => {
                let (class, text) = match status {
                    UpdaterStatus::Checking => ("banner", "checking for updates…".to_owned()),
                    UpdaterStatus::UpToDate { current } => {
                        ("banner", format!("up to date (v{current})"))
                    }
                    UpdaterStatus::Available { latest } => {
                        ("banner", format!("update v{latest} available"))
                    }
                    UpdaterStatus::Downloading { latest } => {
                        ("banner", format!("downloading v{latest}…"))
                    }
                    UpdaterStatus::ReadyToApply { latest } => (
                        "banner",
                        format!(
                            r##"v{latest} downloaded — <button hx-post="/apply-update" hx-target="#status">install</button>"##
                        ),
                    ),
                    UpdaterStatus::RestartRequired { latest } => (
                        "banner",
                        format!(
                            r##"v{latest} installed — <button hx-post="/restart" hx-target="#status">restart soulrust</button>"##
                        ),
                    ),
                    UpdaterStatus::Failed { error } => {
                        ("banner error", format!("update failed: {}", escape(error)))
                    }
                    UpdaterStatus::Skipped { reason } => {
                        ("banner", format!("updates skipped: {}", escape(reason)))
                    }
                };
                format!(r#"<div class="{class}">{text}</div>"#)
            }
        };

        let log = if self.log.is_empty() {
            String::new()
        } else {
            let lines: Vec<String> = self.log.iter().rev().map(|l| escape(l)).collect();
            format!(r#"<pre class="log">{}</pre>"#, lines.join("\n"))
        };

        format!(r#"<div class="banner">{session}</div>{updater}{log}"#)
    }

    fn render_searches(&self) -> String {
        if self.searches.is_empty() {
            return "<p>no searches yet</p>".into();
        }
        self.searches.iter().rev().map(|s| self.render_search(s)).collect()
    }

    /// One search: its query and the per-peer results that cleared the filter,
    /// each result listing the peer (with slot/speed/queue) and its files.
    fn render_search(&self, s: &SearchRow) -> String {
        let total_files: usize = s.results.iter().map(|r| r.files.len()).sum();
        let body = if s.results.is_empty() {
            "<p class=\"muted\">no results yet</p>".to_string()
        } else {
            s.results
                .iter()
                .map(|r| {
                    let slots = if r.free_slots { "free slot" } else { "no free slot" };
                    let files: String = r
                        .files
                        .iter()
                        .map(|(name, size)| {
                            format!(
                                "<li>{} <span class=\"muted\">({} bytes)</span></li>",
                                escape(name),
                                size
                            )
                        })
                        .collect();
                    format!(
                        r#"<div class="result"><strong>{user}</strong> <span class="muted">— {slots}, {speed} B/s, queue {queue}</span><ul>{files}</ul></div>"#,
                        user = escape(&r.username),
                        slots = slots,
                        speed = r.upload_speed,
                        queue = r.in_queue,
                        files = files,
                    )
                })
                .collect()
        };
        format!(
            r#"<div class="card"><h3 style="margin-top:0">{query} <span class="muted">— {peers} peer(s), {files} file(s)</span></h3>{body}</div>"#,
            query = escape(&s.query),
            peers = s.results.len(),
            files = total_files,
            body = body,
        )
    }

    /// The config page itself is rendered by the web bridge (it owns the
    /// current Config via the bus round trip); the Ui only renders a pointer
    /// for the unexpected case it gets asked.
    fn render_config_note(&self) -> String {
        r#"<p>configuration lives at <a href="/config">/config</a></p>"#.into()
    }
}

impl traits::core::Handler for Ui {
    type Id = HandlerId;
    const ID: HandlerId = HandlerId::Ui;
}

impl traits::core::Handle<HttpRender> for Ui {
    fn handle<W: traits::core::Writer>(&mut self, message: &HttpRender, writer: &W) {
        let html = self.render(&message.page);
        Self::send(&HttpHtml { corr: message.corr, html }, writer);
    }
}

impl traits::core::Handle<SessionEvent> for Ui {
    fn handle<W: traits::core::Writer>(&mut self, message: &SessionEvent, _writer: &W) {
        match &message.kind {
            SessionEventKind::Connecting => self.session = SessionStatus::Connecting,
            SessionEventKind::LoggedIn { greeting, own_ip } => {
                self.session = SessionStatus::LoggedIn {
                    greeting: greeting.clone(),
                    own_ip: own_ip.clone(),
                };
            }
            SessionEventKind::LoginFailed { reason } => {
                self.session = SessionStatus::LoginFailed(reason.clone());
            }
            SessionEventKind::Disconnected { reason } => {
                self.session = SessionStatus::Disconnected(reason.clone());
            }
            SessionEventKind::SearchStarted { token, query } => {
                self.searches.push(SearchRow {
                    token: *token,
                    query: query.clone(),
                    results: Vec::new(),
                });
            }
            SessionEventKind::SearchBroadcastSeen { username, query } => {
                self.log(format!("search on the network: {username}: {query}"));
            }
            SessionEventKind::ProtocolNote { note } => self.log(note.clone()),
        }
    }
}

impl traits::core::Handle<UpdaterStatusChanged> for Ui {
    fn handle<W: traits::core::Writer>(&mut self, message: &UpdaterStatusChanged, _writer: &W) {
        self.updater = Some(message.status.clone());
    }
}

impl traits::core::Handle<ConfigChanged> for Ui {
    fn handle<W: traits::core::Writer>(&mut self, message: &ConfigChanged, _writer: &W) {
        self.username = message.config.server.username.clone();
        self.log("configuration updated".into());
    }
}

impl traits::core::Handle<PeerActivity> for Ui {
    fn handle<W: traits::core::Writer>(&mut self, message: &PeerActivity, _writer: &W) {
        self.log(message.note.clone());
    }
}

impl traits::core::Handle<DownloadComplete> for Ui {
    fn handle<W: traits::core::Writer>(&mut self, message: &DownloadComplete, _writer: &W) {
        self.log(format!("downloaded {} from {} → {}", message.filename, message.username, message.path));
    }
}

impl traits::core::Handle<DownloadFailed> for Ui {
    fn handle<W: traits::core::Writer>(&mut self, message: &DownloadFailed, _writer: &W) {
        self.log(format!("download of {} from {} failed: {}", message.filename, message.username, message.reason));
    }
}

impl traits::core::Handle<SearchResultReceived> for Ui {
    fn handle<W: traits::core::Writer>(&mut self, message: &SearchResultReceived, _writer: &W) {
        // Correlate to the search we started; results for an unknown token (not
        // ours, or already cleared) are ignored.
        let Some(row) = self.searches.iter_mut().find(|s| s.token == message.token) else {
            return;
        };
        if row.results.len() >= MAX_RESULTS_PER_SEARCH {
            return;
        }
        row.results.push(SearchResultRow {
            username: message.username.clone(),
            free_slots: message.free_slots,
            upload_speed: message.upload_speed,
            in_queue: message.in_queue,
            files: message.files.iter().map(|f| (f.name.clone(), f.size)).collect(),
        });
    }
}

impl traits::core::Handle<DownloadQueuePosition> for Ui {
    fn handle<W: traits::core::Writer>(&mut self, message: &DownloadQueuePosition, _writer: &W) {
        if message.place == 0 {
            self.log(format!("{} is starting our download of {}", message.username, message.filename));
        } else {
            self.log(format!(
                "download of {} from {} is at queue position {}",
                message.filename, message.username, message.place
            ));
        }
    }
}

impl traits::core::Handle<UploadComplete> for Ui {
    fn handle<W: traits::core::Writer>(&mut self, message: &UploadComplete, _writer: &W) {
        self.log(format!("uploaded {} to {}", message.filename, message.username));
    }
}

impl traits::core::Handle<UploadFailed> for Ui {
    fn handle<W: traits::core::Writer>(&mut self, message: &UploadFailed, _writer: &W) {
        self.log(format!("upload of {} to {} failed: {}", message.filename, message.username, message.reason));
    }
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
    use crate::config::Config;

    fn test_ui() -> Ui {
        let mut config = Config::default();
        config.server.username = "alice".into();
        let ctx = AppContext::new(config, "/tmp/unused.yaml".into());
        struct NullWriter;
        impl Clone for NullWriter {
            fn clone(&self) -> Self {
                NullWriter
            }
        }
        impl traits::core::Writer for NullWriter {
            fn write<
                M: traits::core::Message,
                H: traits::core::Handler,
                F: FnOnce(&mut [u8]),
            >(
                &self,
                _size: usize,
                _callback: F,
            ) {
            }
        }
        Ui::new(&ctx, &NullWriter)
    }

    fn apply(ui: &mut Ui, kind: SessionEventKind) {
        struct NullWriter;
        impl Clone for NullWriter {
            fn clone(&self) -> Self {
                NullWriter
            }
        }
        impl traits::core::Writer for NullWriter {
            fn write<
                M: traits::core::Message,
                H: traits::core::Handler,
                F: FnOnce(&mut [u8]),
            >(
                &self,
                _size: usize,
                _callback: F,
            ) {
            }
        }
        traits::core::Handle::<SessionEvent>::handle(ui, &SessionEvent { kind }, &NullWriter);
    }

    #[test]
    fn index_page_wires_htmx_polling() {
        let ui = test_ui();
        let html = ui.render(&Page::Index);
        assert!(html.contains("/assets/htmx.min.js"));
        assert!(html.contains(r#"hx-get="/fragments/status""#));
        assert!(html.contains(r#"hx-post="/search""#));
    }

    #[test]
    fn status_fragment_reflects_session_state() {
        let mut ui = test_ui();
        assert!(ui.render(&Page::StatusFragment).contains("disconnected"));

        apply(&mut ui, SessionEventKind::LoggedIn {
            greeting: "MOTD".into(),
            own_ip: "1.2.3.4".into(),
        });
        let html = ui.render(&Page::StatusFragment);
        assert!(html.contains("logged in as <b>alice</b>"));
        assert!(html.contains("MOTD"));

        apply(&mut ui, SessionEventKind::LoginFailed { reason: "INVALIDPASS".into() });
        assert!(ui.render(&Page::StatusFragment).contains("login failed: INVALIDPASS"));
    }

    #[test]
    fn searches_fragment_lists_started_searches_newest_first() {
        let mut ui = test_ui();
        assert!(ui.render(&Page::SearchesFragment).contains("no searches"));

        apply(&mut ui, SessionEventKind::SearchStarted { token: 1, query: "first".into() });
        apply(&mut ui, SessionEventKind::SearchStarted { token: 2, query: "second".into() });
        let html = ui.render(&Page::SearchesFragment);
        let first = html.find("first").unwrap();
        let second = html.find("second").unwrap();
        assert!(second < first, "newest search renders first");
    }

    #[test]
    fn html_in_user_data_is_escaped() {
        let mut ui = test_ui();
        apply(&mut ui, SessionEventKind::SearchStarted {
            token: 1,
            query: "<script>alert(1)</script>".into(),
        });
        let html = ui.render(&Page::SearchesFragment);
        assert!(!html.contains("<script>alert"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn search_results_are_correlated_by_token_and_rendered() {
        use crate::messages::SearchResultFile;
        struct NullWriter;
        impl Clone for NullWriter {
            fn clone(&self) -> Self {
                NullWriter
            }
        }
        impl traits::core::Writer for NullWriter {
            fn write<M: traits::core::Message, H: traits::core::Handler, F: FnOnce(&mut [u8])>(
                &self,
                _size: usize,
                _callback: F,
            ) {
            }
        }

        let mut ui = test_ui();
        apply(&mut ui, SessionEventKind::SearchStarted { token: 5, query: "gwen".into() });

        // A result for our search (token 5) is attached and rendered.
        let result = SearchResultReceived {
            token: 5,
            username: "bob".into(),
            free_slots: true,
            upload_speed: 4096,
            in_queue: 0,
            files: vec![SearchResultFile { name: "Music\\Gwen\\hit.mp3".into(), size: 123 }],
        };
        traits::core::Handle::<SearchResultReceived>::handle(&mut ui, &result, &NullWriter);
        let html = ui.render(&Page::SearchesFragment);
        assert!(html.contains("bob"), "peer username rendered");
        assert!(html.contains("Music\\Gwen\\hit.mp3"), "result file rendered");
        assert!(html.contains("1 peer(s)"));

        // A result for a search we never started is ignored (token correlation).
        let stray = SearchResultReceived {
            token: 999,
            username: "eve".into(),
            free_slots: false,
            upload_speed: 0,
            in_queue: 0,
            files: vec![SearchResultFile { name: "spam".into(), size: 1 }],
        };
        traits::core::Handle::<SearchResultReceived>::handle(&mut ui, &stray, &NullWriter);
        assert!(!ui.render(&Page::SearchesFragment).contains("eve"), "unknown-token result dropped");
    }
}
