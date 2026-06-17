//! The UI component: holds the view state and renders htmx pages/fragments.
//! It is the single consumer of all view-relevant events; the web bridge
//! turns HTTP requests into [`HttpRender`] messages and serves whatever HTML
//! comes back.

use std::collections::VecDeque;

use rust_messenger::traits;
use rust_messenger::traits::extended::Sender;

use crate::config::AppContext;
use crate::messages::{
    CancelDownload, ConfigChanged, DownloadComplete, DownloadFailed, DownloadQueuePosition,
    HandlerId, HttpHtml, HttpRender, Page, PeerActivity, SearchResultReceived, SessionEvent,
    SessionEventKind, StartDownload, UpdaterStatus, UpdaterStatusChanged, UploadComplete,
    UploadFailed,
};

const MAX_LOG_LINES: usize = 100;
/// Cap on results kept per search, so a flood of responses can't grow the UI
/// state without bound (filtering already drops the worst before they arrive).
const MAX_RESULTS_PER_SEARCH: usize = 200;
/// Cap on tracked downloads shown on the Downloads page.
const MAX_DOWNLOADS: usize = 200;

#[derive(Debug, Clone, PartialEq)]
enum SessionStatus {
    Disconnected(String),
    Connecting,
    LoggedIn { greeting: String, own_ip: String },
    LoginFailed(String),
}

/// One file within a peer's response, with the audio attributes it advertised.
struct ResultFile {
    name: String,
    size: u64,
    bitrate: Option<u32>,
    length: Option<u32>,
    vbr: bool,
    sample_rate: Option<u32>,
    bit_depth: Option<u32>,
}

/// One peer's filter-passing response to a search.
struct SearchResultRow {
    username: String,
    free_slots: bool,
    upload_speed: u32,
    in_queue: u32,
    files: Vec<ResultFile>,
}

struct SearchRow {
    token: u32,
    query: String,
    results: Vec<SearchResultRow>,
}

/// A user-requested download and its latest known state, for the Downloads page.
struct DownloadEntry {
    username: String,
    filename: String,
    state: DownloadState,
}

#[derive(PartialEq)]
enum DownloadState {
    /// Requested; we're resolving the peer / waiting for it to offer the file.
    Queued,
    /// Sitting in the uploader's queue at this position.
    Position(u32),
    /// The uploader is about to send (queue position 0).
    Starting,
    /// Finished — holds the final on-disk path.
    Completed(String),
    /// Gave up — holds the reason.
    Failed(String),
    /// A partial file found on disk at startup (resumable by re-requesting).
    Incomplete,
}

impl DownloadState {
    /// In flight — belongs in the "Active" section and can be cancelled.
    fn is_active(&self) -> bool {
        matches!(
            self,
            DownloadState::Queued | DownloadState::Position(_) | DownloadState::Starting
        )
    }
}

/// A column the results table can be sorted by. Mirrors the visible columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortKey {
    User,
    Folder,
    File,
    Size,
    Bitrate,
    Length,
    Slot,
    Speed,
    Queue,
}

impl SortKey {
    fn parse(s: &str) -> Option<SortKey> {
        Some(match s {
            "user" => SortKey::User,
            "folder" => SortKey::Folder,
            "file" => SortKey::File,
            "size" => SortKey::Size,
            "bitrate" => SortKey::Bitrate,
            "length" => SortKey::Length,
            "slot" => SortKey::Slot,
            "speed" => SortKey::Speed,
            "queue" => SortKey::Queue,
            _ => return None,
        })
    }
}

pub struct Ui {
    session: SessionStatus,
    searches: Vec<SearchRow>,
    updater: Option<UpdaterStatus>,
    log: VecDeque<String>,
    username: String,
    /// Active results sort: column + descending flag. `None` = arrival order.
    sort: Option<(SortKey, bool)>,
    /// Minimum bitrate (kbps) a result must advertise to be shown; 0 = no filter.
    min_bitrate: u32,
    /// User-requested downloads, newest last, for the Downloads page.
    downloads: Vec<DownloadEntry>,
}

impl Ui {
    pub fn new<W: traits::core::Writer>(ctx: &AppContext, _writer: &W) -> Self {
        Ui {
            session: SessionStatus::Disconnected("starting up".into()),
            searches: Vec::new(),
            updater: None,
            log: VecDeque::new(),
            username: ctx.config.server.username.clone(),
            sort: None,
            min_bitrate: 0,
            // Seed the list from disk so finished and partial downloads from
            // previous runs show up (the in-memory state itself isn't persisted).
            downloads: scan_disk_downloads(
                &ctx.config.sharing.download_path(),
                &ctx.config.sharing.incomplete_path(),
            ),
        }
    }

    /// Record the latest state of a download (keyed by user + virtual path),
    /// inserting it if new. Bounds memory by evicting the oldest finished entry.
    fn set_download_state(&mut self, username: &str, filename: &str, state: DownloadState) {
        if let Some(d) = self
            .downloads
            .iter_mut()
            .find(|d| d.username == username && d.filename == filename)
        {
            d.state = state;
            return;
        }
        self.downloads.push(DownloadEntry {
            username: username.to_owned(),
            filename: filename.to_owned(),
            state,
        });
        if self.downloads.len() > MAX_DOWNLOADS {
            let evict = self
                .downloads
                .iter()
                .position(|d| !d.state.is_active())
                .unwrap_or(0);
            self.downloads.remove(evict);
        }
    }

    /// Click a column header: sort by it ascending, or flip direction if it is
    /// already the active column.
    fn toggle_sort(&mut self, key: SortKey) {
        self.sort = match self.sort {
            Some((k, desc)) if k == key => Some((key, !desc)),
            _ => Some((key, false)),
        };
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
            Page::AccountStatus => self.render_account_status(),
            Page::Downloads => self.render_downloads_page(),
            Page::DownloadsFragment => self.render_downloads(),
            // The sort/filter mutation happens in the HttpRender handler (it has
            // &mut self); here we just render the updated table.
            Page::SortSearches { .. } | Page::FilterBitrate { .. } => self.render_searches(),
        }
    }

    /// Friendly login state for the account screen: connection result plus, on
    /// failure, guidance (notably that INVALIDPASS usually means the username is
    /// taken and a different one will create a new account).
    fn render_account_status(&self) -> String {
        match &self.session {
            SessionStatus::LoggedIn { greeting, .. } => {
                let note = if greeting.trim().is_empty() {
                    String::new()
                } else {
                    format!(r#" <span class="muted">{}</span>"#, escape(greeting))
                };
                format!(
                    r#"<div class="card"><span class="pill ok">● Signed in</span>
<p style="margin:0.6rem 0 0">Connected as <strong>{}</strong>.{note}</p></div>"#,
                    escape(&self.username)
                )
            }
            SessionStatus::Connecting => {
                r#"<div class="card"><span class="pill">● Connecting…</span></div>"#.into()
            }
            SessionStatus::Disconnected(reason) => format!(
                r#"<div class="card"><span class="pill warn">● Not connected</span>
<p class="muted" style="margin:0.6rem 0 0">{}</p></div>"#,
                escape(reason)
            ),
            SessionStatus::LoginFailed(reason) => {
                let hint = if reason.contains("INVALIDPASS") {
                    "That username is already taken, or the password is wrong. To <strong>create a new account</strong>, choose a username nobody has used yet and pick any password — it's registered on first sign-in."
                } else if reason.contains("INVALIDUSERNAME") {
                    "That username isn't allowed — try a simpler one (letters and numbers)."
                } else {
                    "Check the username and password and try again."
                };
                format!(
                    r#"<div class="card"><span class="pill warn">● Sign-in failed</span>
<p style="margin:0.6rem 0 0.3rem"><strong>{}</strong></p><p class="muted" style="margin:0">{hint}</p></div>"#,
                    escape(reason)
                )
            }
        }
    }

    fn render_index(&self) -> String {
        let body = format!(
            r##"<h1>Search</h1>
<p class="sub">Search the Soulseek network or browse a user's shared files. For many tracks at once, use <a href="/bulk">Bulk downloads</a>.</p>
<div id="status" hx-get="/fragments/status" hx-trigger="load, every 2s"></div>
<div class="card">
<form hx-post="/search" hx-target="#searches" hx-swap="innerHTML" style="display:flex; gap:0.5rem; align-items:flex-end;">
  <div style="flex:1"><label for="q" style="margin-top:0">Search</label>
  <input id="q" type="text" name="input" placeholder="search text, or a spotify playlist / album / track link" autofocus></div>
  <button class="btn" type="submit">Search</button>
</form>
</div>
{col_bar}
<div id="searches" class="results" hx-get="/fragments/searches" hx-trigger="load, every 2s"></div>
<h2>Browse a user's shares</h2>
<div class="card">
<form hx-post="/browse" hx-target="#browse" hx-swap="innerHTML" style="display:flex; gap:0.5rem; align-items:flex-end;">
  <div style="flex:1"><label for="u" style="margin-top:0">Username</label>
  <input id="u" type="text" name="username" placeholder="soulseek username"></div>
  <button class="btn secondary" type="submit">Browse</button>
</form>
</div>
<div id="browse" hx-get="/fragments/browse" hx-trigger="load, every 3s"></div>"##,
            col_bar = crate::components::ui_theme::col_bar(self.min_bitrate),
        );
        crate::components::ui_theme::shell("soulrust", "search", &self.username, &body)
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

    /// The action cell for a result row. If the file is already in the downloads
    /// list, show its live status (so the 2s poll preserves it instead of
    /// reverting to "Get"); otherwise a Get button that posts the download.
    /// Failed downloads fall through to a Get button so they can be retried.
    fn download_cell(&self, username: &str, filename: &str, size: u64) -> String {
        let state = self
            .downloads
            .iter()
            .find(|d| d.username == username && d.filename == filename)
            .map(|d| &d.state);
        // An in-flight download shows its status plus a cancel control; the
        // cancel posts to /download/cancel and the cell swaps back to a Get
        // button. Done/failed/never-requested fall through to a Get button.
        let cancellable = |pill: &str| {
            format!(
                r##"<form hx-post="/download/cancel" hx-target="closest td" hx-swap="innerHTML" style="margin:0;display:inline-flex;gap:0.3rem;align-items:center"><input type="hidden" name="username" value="{user}"><input type="hidden" name="filename" value="{path}"><input type="hidden" name="size" value="{size}">{pill}<button class="btn xs secondary" type="submit" title="Cancel">✕</button></form>"##,
                user = escape(username),
                path = escape(filename),
                size = size,
                pill = pill,
            )
        };
        match state {
            Some(DownloadState::Queued) => cancellable(r#"<span class="pill">queued</span>"#),
            Some(DownloadState::Position(n)) => {
                cancellable(&format!(r#"<span class="pill">queue #{n}</span>"#))
            }
            Some(DownloadState::Starting) => {
                cancellable(r#"<span class="pill ok">downloading…</span>"#)
            }
            Some(DownloadState::Completed(_)) => r#"<span class="pill ok">done</span>"#.into(),
            _ => format!(
                r##"<form hx-post="/download" hx-target="this" hx-swap="outerHTML" style="margin:0"><input type="hidden" name="username" value="{user}"><input type="hidden" name="filename" value="{path}"><input type="hidden" name="size" value="{size}"><button class="btn xs" type="submit">Get</button></form>"##,
                user = escape(username),
                path = escape(filename),
                size = size,
            ),
        }
    }

    /// A clickable, sort-aware column header cell. `id` is the sort key sent to
    /// `/sort/{id}`; the active column shows a ▲/▼ arrow. `hx-target` is the
    /// enclosing `.results` container so this works on both the search and bulk
    /// pages (each has its own polled results div).
    fn sort_th(&self, id: &str, title: &str, num: bool) -> String {
        let arrow = match self.sort {
            Some((k, desc)) if SortKey::parse(id) == Some(k) => {
                if desc { " ▼" } else { " ▲" }
            }
            _ => "",
        };
        let active = if arrow.is_empty() { "" } else { " sorted" };
        format!(
            r##"<th class="col-{id}{numc}{active}"><a class="sort" hx-get="/sort/{id}" hx-target="closest .results" hx-swap="innerHTML">{title}{arrow}</a></th>"##,
            id = id,
            numc = if num { " num" } else { "" },
            active = active,
            title = escape(title),
            arrow = arrow,
        )
    }

    /// One search rendered as a dense, both-axis scrollable table: one row per
    /// (peer, file). Columns mirror Nicotine+ (User, Folder, File, Size,
    /// Bitrate, Length, plus slot/speed/queue and a Download button). Headers
    /// sort the rows; the bitrate filter drops rows below `min_bitrate`. Column
    /// visibility is driven by the CSS-only `.col-bar` toggles on the page
    /// (outside this polled fragment, so their state survives the 2s refresh).
    fn render_search(&self, s: &SearchRow) -> String {
        let total_files: usize = s.results.iter().map(|r| r.files.len()).sum();
        if s.results.is_empty() {
            return format!(
                r#"<div class="card"><h3 style="margin-top:0">{query} <span class="muted">— no results yet</span></h3></div>"#,
                query = escape(&s.query),
            );
        }
        // Flatten to (peer, file) rows, applying the bitrate filter.
        let mut rows: Vec<(&SearchResultRow, &ResultFile)> = s
            .results
            .iter()
            .flat_map(|r| r.files.iter().map(move |f| (r, f)))
            .filter(|(_, f)| self.min_bitrate == 0 || effective_bitrate(f).is_some_and(|b| b >= self.min_bitrate))
            .collect();
        if let Some((key, desc)) = self.sort {
            rows.sort_by(|a, b| sort_cmp(key, a, b));
            if desc {
                rows.reverse();
            }
        }
        let shown = rows.len();
        let body: String = rows
            .iter()
            .map(|(r, f)| {
                let slot = if r.free_slots {
                    r#"<span class="pill ok">free</span>"#
                } else {
                    r#"<span class="pill warn">queued</span>"#
                };
                let length = f.length.map(length_str).unwrap_or_default();
                format!(
                    r##"<tr><td class="col-user" title="{user}">{user}</td><td class="col-folder" title="{folder}">{folder}</td><td class="col-file" title="{path}">{file}</td><td class="col-size num">{human}</td><td class="col-bitrate num">{quality}</td><td class="col-length num">{length}</td><td class="col-slot">{slot}</td><td class="col-speed num">{speed}</td><td class="col-queue num">{queue}</td><td class="col-dl">{dl_cell}</td></tr>"##,
                    user = escape(&r.username),
                    folder = escape(dirname(&f.name)),
                    path = escape(&f.name),
                    file = escape(basename(&f.name)),
                    human = human_size(f.size),
                    quality = escape(&quality_str(f)),
                    length = escape(&length),
                    slot = slot,
                    speed = r.upload_speed,
                    queue = r.in_queue,
                    dl_cell = self.download_cell(&r.username, &f.name, f.size),
                )
            })
            .collect();
        let count = if shown == total_files {
            format!("{peers} peer(s), {total_files} file(s)", peers = s.results.len())
        } else {
            format!("{shown} of {total_files} file(s) — bitrate filter active")
        };
        format!(
            r##"<div class="card"><h3 style="margin-top:0">{query} <span class="muted">— {count}</span></h3><div class="results-scroll"><table class="results-table"><thead><tr>{th_user}{th_folder}{th_file}{th_size}{th_bitrate}{th_length}{th_slot}{th_speed}{th_queue}<th class="col-dl"></th></tr></thead><tbody>{body}</tbody></table></div></div>"##,
            query = escape(&s.query),
            count = count,
            th_user = self.sort_th("user", "User", false),
            th_folder = self.sort_th("folder", "Folder", false),
            th_file = self.sort_th("file", "File", false),
            th_size = self.sort_th("size", "Size", true),
            th_bitrate = self.sort_th("bitrate", "Bitrate", true),
            th_length = self.sort_th("length", "Length", true),
            th_slot = self.sort_th("slot", "Slot", false),
            th_speed = self.sort_th("speed", "Speed B/s", true),
            th_queue = self.sort_th("queue", "Queue", true),
            body = body,
        )
    }

    /// The full Downloads page: a shell around the live downloads fragment.
    fn render_downloads_page(&self) -> String {
        let body = r##"<h1>Downloads</h1>
<p class="sub">Files you've queued with the <strong>Get</strong> button. Active transfers are up top; finished and failed ones below.</p>
<div id="downloads" class="results" hx-get="/fragments/downloads" hx-trigger="load, every 2s"></div>"##;
        crate::components::ui_theme::shell("soulrust — downloads", "downloads", &self.username, body)
    }

    /// One downloads table. `active` selects the in-flight rows (true) or the
    /// at-rest ones — completed, failed, or partial-on-disk (false).
    fn render_downloads_table(&self, active: bool) -> String {
        let rows: String = self
            .downloads
            .iter()
            .filter(|d| d.state.is_active() == active)
            .rev()
            .map(|d| {
                let status = match &d.state {
                    DownloadState::Queued => r#"<span class="pill">queued</span>"#.to_string(),
                    DownloadState::Position(n) => {
                        format!(r#"<span class="pill">queue #{n}</span>"#)
                    }
                    DownloadState::Starting => {
                        r#"<span class="pill ok">downloading…</span>"#.to_string()
                    }
                    DownloadState::Completed(path) => format!(
                        r#"<span class="pill ok">done</span> <span class="muted">{}</span>"#,
                        escape(path)
                    ),
                    DownloadState::Failed(reason) => format!(
                        r#"<span class="pill warn">failed</span> <span class="muted">{}</span>"#,
                        escape(reason)
                    ),
                    DownloadState::Incomplete => {
                        r#"<span class="pill warn">incomplete</span> <span class="muted">partial file on disk — search and Get again to resume</span>"#.to_string()
                    }
                };
                // In-flight rows can be cancelled (removes the row); at-rest ones can't.
                let action = if active {
                    format!(
                        r##" <form hx-post="/download/cancel" hx-target="closest tr" hx-swap="delete" style="margin:0;display:inline"><input type="hidden" name="username" value="{user}"><input type="hidden" name="filename" value="{path}"><button class="btn xs secondary" type="submit">cancel</button></form>"##,
                        user = escape(&d.username),
                        path = escape(&d.filename),
                    )
                } else {
                    String::new()
                };
                let user = if d.username.is_empty() {
                    r#"<span class="muted">—</span>"#.to_string()
                } else {
                    escape(&d.username)
                };
                format!(
                    r##"<tr><td class="col-file" title="{path}">{file}</td><td class="col-user">{user}</td><td>{status}{action}</td></tr>"##,
                    path = escape(&d.filename),
                    file = escape(basename(&d.filename)),
                    user = user,
                    status = status,
                    action = action,
                )
            })
            .collect();
        if rows.is_empty() {
            let what = if active { "nothing downloading right now" } else { "no finished downloads yet" };
            return format!(r#"<p class="muted">{what}</p>"#);
        }
        format!(
            r##"<div class="results-scroll"><table class="results-table"><thead><tr><th class="col-file">File</th><th class="col-user">User</th><th>Status</th></tr></thead><tbody>{rows}</tbody></table></div>"##,
        )
    }

    /// The live downloads fragment: an Active section then a Previous section.
    fn render_downloads(&self) -> String {
        let active = self.downloads.iter().filter(|d| d.state.is_active()).count();
        let done = self.downloads.len() - active;
        format!(
            r##"<div class="card"><h3 style="margin-top:0">Active <span class="muted">— {active}</span></h3>{active_tbl}</div><div class="card"><h3 style="margin-top:0">Previous <span class="muted">— {done}</span></h3>{prev_tbl}</div>"##,
            active = active,
            done = done,
            active_tbl = self.render_downloads_table(true),
            prev_tbl = self.render_downloads_table(false),
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
        // Sort/filter pages mutate view state before rendering.
        match &message.page {
            Page::SortSearches { key } => {
                if let Some(k) = SortKey::parse(key) {
                    self.toggle_sort(k);
                }
            }
            Page::FilterBitrate { min } => self.min_bitrate = *min,
            _ => {}
        }
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

impl traits::core::Handle<StartDownload> for Ui {
    fn handle<W: traits::core::Writer>(&mut self, message: &StartDownload, _writer: &W) {
        self.set_download_state(&message.username, &message.filename, DownloadState::Queued);
    }
}

impl traits::core::Handle<CancelDownload> for Ui {
    fn handle<W: traits::core::Writer>(&mut self, message: &CancelDownload, _writer: &W) {
        self.downloads
            .retain(|d| !(d.username == message.username && d.filename == message.filename));
    }
}

impl traits::core::Handle<DownloadComplete> for Ui {
    fn handle<W: traits::core::Writer>(&mut self, message: &DownloadComplete, _writer: &W) {
        self.log(format!("downloaded {} from {} → {}", message.filename, message.username, message.path));
        self.set_download_state(
            &message.username,
            &message.filename,
            DownloadState::Completed(message.path.clone()),
        );
    }
}

impl traits::core::Handle<DownloadFailed> for Ui {
    fn handle<W: traits::core::Writer>(&mut self, message: &DownloadFailed, _writer: &W) {
        self.log(format!("download of {} from {} failed: {}", message.filename, message.username, message.reason));
        self.set_download_state(
            &message.username,
            &message.filename,
            DownloadState::Failed(message.reason.clone()),
        );
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
            files: message
                .files
                .iter()
                .map(|f| ResultFile {
                    name: f.name.clone(),
                    size: f.size,
                    bitrate: f.bitrate,
                    length: f.length,
                    vbr: f.vbr,
                    sample_rate: f.sample_rate,
                    bit_depth: f.bit_depth,
                })
                .collect(),
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
        // Don't resurrect a finished/at-rest entry from a late queue update.
        let updatable = self
            .downloads
            .iter()
            .find(|d| d.username == message.username && d.filename == message.filename)
            .is_none_or(|d| d.state.is_active());
        if updatable {
            let state = if message.place == 0 {
                DownloadState::Starting
            } else {
                DownloadState::Position(message.place)
            };
            self.set_download_state(&message.username, &message.filename, state);
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

/// The last path segment of a Soulseek virtual path (backslash- or
/// slash-separated), for the dense File column. The full path stays in the
/// row's `title` and the hidden download field.
fn basename(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().filter(|s| !s.is_empty()).unwrap_or(path)
}

/// Reconstruct the Downloads list from disk at startup: finished files in the
/// download folder (shown as "done") and `INCOMPLETE-…` partials in the
/// incomplete folder (shown as "incomplete", resumable). We can only recover the
/// basename from disk — not the original peer or virtual path — so those are
/// left blank. Bounded to [`MAX_DOWNLOADS`].
fn scan_disk_downloads(download_dir: &std::path::Path, incomplete_dir: &std::path::Path) -> Vec<DownloadEntry> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(download_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Skip the incomplete subfolder and any stray partials.
            if !entry.path().is_file() || name.starts_with("INCOMPLETE-") {
                continue;
            }
            out.push(DownloadEntry {
                username: String::new(),
                filename: name,
                state: DownloadState::Completed(entry.path().display().to_string()),
            });
        }
    }
    if let Ok(entries) = std::fs::read_dir(incomplete_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(basename) = parse_incomplete_name(&name) {
                out.push(DownloadEntry {
                    username: String::new(),
                    filename: basename,
                    state: DownloadState::Incomplete,
                });
            }
        }
    }
    out.truncate(MAX_DOWNLOADS);
    out
}

/// Recover the original basename from an `INCOMPLETE-<16hex>-<basename>` partial
/// (see `peer_net::incomplete_name`). Returns `None` if the name isn't one.
fn parse_incomplete_name(name: &str) -> Option<String> {
    let rest = name.strip_prefix("INCOMPLETE-")?;
    // 16 hex digits of the stable key, then '-', then the basename.
    if rest.len() > 17 && rest.as_bytes()[16] == b'-' && rest[..16].bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(rest[17..].to_string())
    } else {
        None
    }
}

/// The folder portion of a Soulseek virtual path (everything before the last
/// separator), for the Folder column.
fn dirname(path: &str) -> &str {
    match path.rfind(['\\', '/']) {
        Some(i) => &path[..i],
        None => "",
    }
}

/// Bitrate to sort/filter on: the advertised value, or — for lossless audio
/// that only gave sample rate + bit depth — Nicotine+'s estimate
/// (sample_rate × bit_depth × 2 channels / 1000).
fn effective_bitrate(f: &ResultFile) -> Option<u32> {
    f.bitrate.or_else(|| match (f.sample_rate, f.bit_depth) {
        (Some(sr), Some(bd)) => Some(sr.saturating_mul(bd).saturating_mul(2) / 1000),
        _ => None,
    })
}

/// The "Quality" cell, matching Nicotine+: `"44.1 kHz / 16 bit"` for lossless,
/// else `"320 kbps"` (with ` (vbr)` for variable bitrate), else empty.
fn quality_str(f: &ResultFile) -> String {
    if let (Some(sr), Some(bd)) = (f.sample_rate, f.bit_depth) {
        format!("{} kHz / {} bit", khz(sr), bd)
    } else if let Some(br) = f.bitrate {
        if f.vbr {
            format!("{br} kbps (vbr)")
        } else {
            format!("{br} kbps")
        }
    } else {
        String::new()
    }
}

/// Sample rate in Hz to a compact kHz string (e.g. 44100 → `44.1`, 48000 → `48`).
fn khz(sample_rate: u32) -> String {
    let v = sample_rate as f64 / 1000.0;
    if (v.fract()).abs() < 1e-9 {
        format!("{}", v as u32)
    } else {
        format!("{v:.1}")
    }
}

/// Seconds as `M:SS` (or `H:MM:SS` past an hour).
fn length_str(secs: u32) -> String {
    let (m, s) = (secs / 60, secs % 60);
    let (h, m) = (m / 60, m % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Order two flattened result rows by the active column.
fn sort_cmp(
    key: SortKey,
    a: &(&SearchResultRow, &ResultFile),
    b: &(&SearchResultRow, &ResultFile),
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (ar, af) = a;
    let (br, bf) = b;
    match key {
        SortKey::User => ar.username.to_lowercase().cmp(&br.username.to_lowercase()),
        SortKey::Folder => dirname(&af.name).to_lowercase().cmp(&dirname(&bf.name).to_lowercase()),
        SortKey::File => basename(&af.name).to_lowercase().cmp(&basename(&bf.name).to_lowercase()),
        SortKey::Size => af.size.cmp(&bf.size),
        SortKey::Bitrate => effective_bitrate(af).cmp(&effective_bitrate(bf)),
        SortKey::Length => af.length.cmp(&bf.length),
        SortKey::Slot => ar.free_slots.cmp(&br.free_slots),
        SortKey::Speed => ar.upload_speed.cmp(&br.upload_speed),
        SortKey::Queue => ar.in_queue.cmp(&br.in_queue),
    }
    .then(Ordering::Equal)
}

/// Bytes as a compact human-readable size (e.g. `4.2 MB`).
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn human_size_is_compact_and_unit_scaled() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(4_404_019), "4.2 MB");
    }

    #[test]
    fn basename_takes_the_last_segment_of_either_separator() {
        assert_eq!(basename("Music\\Gwen\\hit.mp3"), "hit.mp3");
        assert_eq!(basename("a/b/c.flac"), "c.flac");
        assert_eq!(basename("loose.mp3"), "loose.mp3");
    }

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
        assert!(html.contains("/assets/app.js"), "scroll-preservation script is loaded");
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
    fn account_status_guides_account_creation_on_invalid_pass() {
        let mut ui = test_ui();
        apply(&mut ui, SessionEventKind::LoginFailed { reason: "INVALIDPASS".into() });
        let html = ui.render(&Page::AccountStatus);
        assert!(html.contains("Sign-in failed"));
        assert!(html.contains("create a new account"), "explains how to make an account");

        apply(&mut ui, SessionEventKind::LoggedIn {
            greeting: "hi".into(),
            own_ip: "1.2.3.4".into(),
        });
        let ok = ui.render(&Page::AccountStatus);
        assert!(ok.contains("Signed in") && ok.contains("alice"));
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
            files: vec![SearchResultFile {
                name: "Music\\Gwen\\hit.mp3".into(),
                size: 123,
                bitrate: Some(320),
                length: Some(184),
                vbr: false,
                sample_rate: None,
                bit_depth: None,
            }],
        };
        traits::core::Handle::<SearchResultReceived>::handle(&mut ui, &result, &NullWriter);
        let html = ui.render(&Page::SearchesFragment);
        assert!(html.contains("bob"), "peer username rendered");
        assert!(html.contains("Music\\Gwen\\hit.mp3"), "full path kept for download/title");
        assert!(html.contains("1 peer(s)"));
        // Dense table: column classes drive the CSS show/hide toggles.
        assert!(html.contains(r#"class="results-table""#), "rendered as a table");
        assert!(html.contains(r#"class="col-user""#) && html.contains(r#"class="col-speed num""#));
        // The File column shows the basename, the Folder column the dir.
        assert!(html.contains(">hit.mp3<"), "file column shows the basename");
        assert!(html.contains(r#"class="col-folder" title="Music\Gwen""#), "folder column");
        // Audio attributes surface as Bitrate + Length columns.
        assert!(html.contains("320 kbps"), "bitrate column rendered");
        assert!(html.contains("3:04"), "length column rendered (184s)");
        // Headers are clickable sort controls.
        assert!(html.contains(r#"hx-get="/sort/bitrate""#), "sortable bitrate header");
        // Each row carries a download form posting the exact path + size.
        assert!(html.contains(r#"hx-post="/download""#), "row has a download action");
        assert!(html.contains(r#"name="filename" value="Music\Gwen\hit.mp3""#));
        assert!(html.contains(r#"name="size" value="123""#));

        // A result for a search we never started is ignored (token correlation).
        let stray = SearchResultReceived {
            token: 999,
            username: "eve".into(),
            free_slots: false,
            upload_speed: 0,
            in_queue: 0,
            files: vec![SearchResultFile {
                name: "spam".into(),
                size: 1,
                bitrate: None,
                length: None,
                vbr: false,
                sample_rate: None,
                bit_depth: None,
            }],
        };
        traits::core::Handle::<SearchResultReceived>::handle(&mut ui, &stray, &NullWriter);
        assert!(!ui.render(&Page::SearchesFragment).contains("eve"), "unknown-token result dropped");
    }

    /// Feed one single-file result into a search via the real handler path.
    fn feed_result(ui: &mut Ui, token: u32, user: &str, name: &str, bitrate: Option<u32>) {
        use crate::messages::SearchResultFile;
        struct W;
        impl Clone for W {
            fn clone(&self) -> Self {
                W
            }
        }
        impl traits::core::Writer for W {
            fn write<M: traits::core::Message, H: traits::core::Handler, F: FnOnce(&mut [u8])>(
                &self,
                _size: usize,
                _callback: F,
            ) {
            }
        }
        let msg = SearchResultReceived {
            token,
            username: user.into(),
            free_slots: true,
            upload_speed: 0,
            in_queue: 0,
            files: vec![SearchResultFile {
                name: name.into(),
                size: 1,
                bitrate,
                length: None,
                vbr: false,
                sample_rate: None,
                bit_depth: None,
            }],
        };
        traits::core::Handle::<SearchResultReceived>::handle(ui, &msg, &W);
    }

    #[test]
    fn downloads_page_tracks_active_then_moves_to_previous() {
        struct W;
        impl Clone for W {
            fn clone(&self) -> Self {
                W
            }
        }
        impl traits::core::Writer for W {
            fn write<M: traits::core::Message, H: traits::core::Handler, F: FnOnce(&mut [u8])>(
                &self,
                _size: usize,
                _callback: F,
            ) {
            }
        }
        let mut ui = test_ui();
        assert!(ui.render(&Page::DownloadsFragment).contains("nothing downloading"));

        // Clicking Get → StartDownload → an active "queued" entry.
        traits::core::Handle::<StartDownload>::handle(
            &mut ui,
            &StartDownload { username: "bob".into(), filename: "M\\song.mp3".into(), size: 9 },
            &W,
        );
        let html = ui.render(&Page::DownloadsFragment);
        assert!(html.contains("Active <span class=\"muted\">— 1"));
        assert!(html.contains("song.mp3") && html.contains("queued"));

        // Queue position update keeps it active.
        traits::core::Handle::<DownloadQueuePosition>::handle(
            &mut ui,
            &DownloadQueuePosition { username: "bob".into(), filename: "M\\song.mp3".into(), place: 3 },
            &W,
        );
        assert!(ui.render(&Page::DownloadsFragment).contains("queue #3"));

        // Completion moves it to Previous with the final path.
        traits::core::Handle::<DownloadComplete>::handle(
            &mut ui,
            &DownloadComplete {
                username: "bob".into(),
                filename: "M\\song.mp3".into(),
                path: "/dl/song.mp3".into(),
            },
            &W,
        );
        let html = ui.render(&Page::DownloadsFragment);
        assert!(html.contains("Active <span class=\"muted\">— 0"));
        assert!(html.contains("Previous <span class=\"muted\">— 1"));
        assert!(html.contains("done") && html.contains("/dl/song.mp3"));
    }

    #[test]
    fn cancelling_a_download_removes_it() {
        struct W;
        impl Clone for W {
            fn clone(&self) -> Self {
                W
            }
        }
        impl traits::core::Writer for W {
            fn write<M: traits::core::Message, H: traits::core::Handler, F: FnOnce(&mut [u8])>(
                &self,
                _size: usize,
                _callback: F,
            ) {
            }
        }
        let mut ui = test_ui();
        traits::core::Handle::<StartDownload>::handle(
            &mut ui,
            &StartDownload { username: "bob".into(), filename: "a.mp3".into(), size: 1 },
            &W,
        );
        assert!(ui.render(&Page::DownloadsFragment).contains("Active <span class=\"muted\">— 1"));
        // An active row offers a cancel control.
        assert!(ui.render(&Page::DownloadsFragment).contains(r#"hx-post="/download/cancel""#));

        traits::core::Handle::<CancelDownload>::handle(
            &mut ui,
            &CancelDownload { username: "bob".into(), filename: "a.mp3".into() },
            &W,
        );
        let html = ui.render(&Page::DownloadsFragment);
        assert!(html.contains("Active <span class=\"muted\">— 0"), "cancelled download is gone");
        assert!(html.contains("nothing downloading"));
    }

    #[test]
    fn result_row_shows_queued_status_so_the_poll_keeps_it() {
        struct W;
        impl Clone for W {
            fn clone(&self) -> Self {
                W
            }
        }
        impl traits::core::Writer for W {
            fn write<M: traits::core::Message, H: traits::core::Handler, F: FnOnce(&mut [u8])>(
                &self,
                _size: usize,
                _callback: F,
            ) {
            }
        }
        let mut ui = test_ui();
        apply(&mut ui, SessionEventKind::SearchStarted { token: 8, query: "q".into() });
        feed_result(&mut ui, 8, "carol", "Album\\tune.mp3", Some(256));
        // Before requesting, the row offers a Get button.
        assert!(ui.render(&Page::SearchesFragment).contains(r#"hx-post="/download""#));

        // Requesting it (what clicking Get does) makes the row show the queued
        // status, derived from server state — so the next 2s poll preserves it
        // rather than reverting to a Get button.
        traits::core::Handle::<StartDownload>::handle(
            &mut ui,
            &StartDownload { username: "carol".into(), filename: "Album\\tune.mp3".into(), size: 1 },
            &W,
        );
        let html = ui.render(&Page::SearchesFragment);
        assert!(html.contains(r#"<span class="pill">queued</span>"#), "row shows queued");
    }

    #[test]
    fn parse_incomplete_name_recovers_the_basename() {
        assert_eq!(
            parse_incomplete_name("INCOMPLETE-00000000deadbeef-song.flac").as_deref(),
            Some("song.flac")
        );
        assert_eq!(parse_incomplete_name("ordinary.mp3"), None);
        // 16 chars but not all hex → not one of ours.
        assert_eq!(parse_incomplete_name("INCOMPLETE-zzzzzzzzzzzzzzzz-x.mp3"), None);
    }

    #[test]
    fn scan_disk_downloads_lists_completed_and_incomplete() {
        let base = std::env::temp_dir().join(format!("soulrust-dlscan-{}", std::process::id()));
        let dl = base.join("dl");
        let inc = base.join("inc");
        std::fs::create_dir_all(&dl).unwrap();
        std::fs::create_dir_all(&inc).unwrap();
        std::fs::write(dl.join("done.mp3"), b"x").unwrap();
        std::fs::write(inc.join("INCOMPLETE-00000000deadbeef-half.flac"), b"x").unwrap();
        let entries = scan_disk_downloads(&dl, &inc);
        let _ = std::fs::remove_dir_all(&base);

        assert!(entries.iter().any(|d| d.filename == "done.mp3"
            && matches!(d.state, DownloadState::Completed(_))));
        assert!(entries
            .iter()
            .any(|d| d.filename == "half.flac" && d.state == DownloadState::Incomplete));
    }

    #[test]
    fn results_sort_by_clicked_column_and_filter_by_bitrate() {
        let mut ui = test_ui();
        apply(&mut ui, SessionEventKind::SearchStarted { token: 7, query: "q".into() });
        feed_result(&mut ui, 7, "lo", "low.mp3", Some(128));
        feed_result(&mut ui, 7, "hi", "high.mp3", Some(320));

        // Sort by bitrate ascending: 128 before 320.
        ui.toggle_sort(SortKey::Bitrate);
        let html = ui.render(&Page::SearchesFragment);
        assert!(html.contains("▲"), "active column shows an ascending arrow");
        assert!(html.find("low.mp3").unwrap() < html.find("high.mp3").unwrap(), "ascending");

        // Clicking the same column again flips to descending.
        ui.toggle_sort(SortKey::Bitrate);
        let html = ui.render(&Page::SearchesFragment);
        assert!(html.contains("▼"));
        assert!(html.find("high.mp3").unwrap() < html.find("low.mp3").unwrap(), "descending");

        // Bitrate filter drops the 128 kbps result.
        ui.min_bitrate = 256;
        let html = ui.render(&Page::SearchesFragment);
        assert!(html.contains("high.mp3") && !html.contains("low.mp3"), "filter keeps only ≥256");
        assert!(html.contains("bitrate filter active"));
    }
}
