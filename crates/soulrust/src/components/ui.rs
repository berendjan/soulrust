//! The UI component: holds the view state and renders htmx pages/fragments.
//! It is the single consumer of all view-relevant events; the web bridge
//! turns HTTP requests into [`HttpRender`] messages and serves whatever HTML
//! comes back.

use std::collections::VecDeque;

use rust_messenger::traits;
use rust_messenger::traits::extended::Sender;

use crate::config::AppContext;
use crate::messages::{
    ConfigChanged, HandlerId, HttpHtml, HttpRender, Page, SessionEvent, SessionEventKind,
    UpdaterStatus, UpdaterStatusChanged,
};

const MAX_LOG_LINES: usize = 100;

#[derive(Debug, Clone, PartialEq)]
enum SessionStatus {
    Disconnected(String),
    Connecting,
    LoggedIn { greeting: String, own_ip: String },
    LoginFailed(String),
}

struct SearchRow {
    token: u32,
    query: String,
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
        format!(
            r##"<!DOCTYPE html>
<html>
<head>
<title>soulrust</title>
<script src="/assets/htmx.min.js"></script>
<style>
body {{ font-family: sans-serif; max-width: 56rem; margin: 2rem auto; padding: 0 1rem; }}
table {{ border-collapse: collapse; width: 100%; }}
td, th {{ border-bottom: 1px solid #ddd; padding: 0.4rem; text-align: left; }}
.banner {{ padding: 0.5rem; border-radius: 4px; background: #eef; margin: 0.5rem 0; }}
.error {{ background: #fee; }}
form.search {{ display: flex; gap: 0.5rem; margin: 1rem 0; }}
form.search input[type=text] {{ flex: 1; padding: 0.4rem; }}
pre.log {{ background: #f6f6f6; padding: 0.5rem; max-height: 14rem; overflow-y: auto; }}
</style>
</head>
<body>
<h1>soulrust</h1>
<div id="status" hx-get="/fragments/status" hx-trigger="load, every 2s"></div>
<form class="search" hx-post="/search" hx-target="#searches" hx-swap="innerHTML">
  <input type="text" name="input" placeholder="search text or spotify playlist/album/track link" autofocus>
  <button type="submit">Search</button>
</form>
<div id="searches" hx-get="/fragments/searches" hx-trigger="load, every 2s"></div>
<h2>browse a user's shares</h2>
<form class="search" hx-post="/browse" hx-target="#browse" hx-swap="innerHTML">
  <input type="text" name="username" placeholder="soulseek username">
  <button type="submit">Browse</button>
</form>
<div id="browse" hx-get="/fragments/browse" hx-trigger="load, every 3s"></div>
<p><a href="/config">configuration</a></p>
</body>
</html>"##
        )
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
        let rows: String = self
            .searches
            .iter()
            .rev()
            .map(|s| {
                format!(
                    "<tr><td>{}</td><td>{}</td></tr>",
                    s.token,
                    escape(&s.query)
                )
            })
            .collect();
        format!(
            r#"<table><tr><th>token</th><th>query</th></tr>{rows}</table>"#
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
                self.searches.push(SearchRow { token: *token, query: query.clone() });
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
}
