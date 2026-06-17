//! Shared look-and-feel for the web UI: the page shell (collapsible icon
//! sidebar + nav + footer account chip) and the CSS theme, so every page —
//! search, bulk downloads, Spotify setup, and settings — renders consistently.
//! The styling mirrors the Rootline `dashboard-beta` design: a light,
//! cool-neutral palette, the Inter type stack, generous rounding, subtle ring
//! borders, a 208↔64px collapsible sidebar with per-item icons, and a footer
//! account chip. Collapsing is pure CSS (a checkbox toggled by the logo) — no
//! JavaScript.

/// The full stylesheet, embedded in every page `<head>`.
pub const THEME_CSS: &str = r#"
:root {
  --bg: #fcfbfa; --surface: #ffffff; --muted: #f4f3f1; --muted-2: #efedea;
  --text: #1a1416; --text-soft: #6b6560; --border: #eae8e5; --input: #e2e0dc;
  --primary: #2d2729; --primary-text: #fbfafa; --accent: #1db954;
  --error-bg: #fbeae8; --error-text: #a23a2e; --ok: #1f7a4d;
  --radius: 10px; --radius-pill: 999px; --radius-card: 18px;
  --side-w: 208px; --side-w-collapsed: 64px;
}
* { box-sizing: border-box; }
body {
  margin: 0; background: var(--bg); color: var(--text);
  font-family: 'Inter', system-ui, -apple-system, 'Segoe UI', sans-serif;
  font-size: 14px; line-height: 1.5;
}
a { color: inherit; }
.nav-collapse { position: absolute; opacity: 0; pointer-events: none; }
.layout { display: flex; min-height: 100vh; }
.sidebar {
  width: var(--side-w); flex: 0 0 var(--side-w); background: #f6f5f3;
  border-right: 1px solid var(--border); padding: 0.7rem; position: sticky; top: 0;
  height: 100vh; display: flex; flex-direction: column; overflow: hidden;
  transition: width 0.18s ease, flex-basis 0.18s ease;
}
.brand { display: flex; align-items: center; gap: 0.6rem; height: 42px; padding: 0 0.45rem;
  font-weight: 600; font-size: 15px; cursor: pointer; border-radius: var(--radius); user-select: none; }
.brand:hover { background: var(--muted-2); }
.brand .logo { flex: 0 0 26px; width: 26px; height: 26px; color: var(--primary); display: flex; }
.brand .logo svg { width: 26px; height: 26px; }
.brand-name { white-space: nowrap; }
.nav { margin-top: 0.6rem; display: flex; flex-direction: column; gap: 2px; }
.nav a { height: 38px; display: flex; align-items: center; gap: 0.6rem; padding: 0 0.7rem;
  border-radius: var(--radius-pill); text-decoration: none; color: var(--text-soft);
  font-weight: 500; white-space: nowrap; overflow: hidden; }
.nav a:hover { background: var(--muted-2); color: var(--text); }
.nav a.active { background: rgba(45,39,41,0.06); color: var(--text); }
.ico { flex: 0 0 20px; width: 20px; height: 20px; display: flex; }
.ico svg { width: 20px; height: 20px; }
.label { white-space: nowrap; }
.nav-footer { margin-top: auto; padding-top: 0.6rem; border-top: 1px solid var(--border); }
.user { display: flex; align-items: center; gap: 0.6rem; height: 44px; padding: 0 0.5rem;
  border-radius: var(--radius-pill); text-decoration: none; color: var(--text); overflow: hidden; }
.user:hover { background: var(--muted-2); }
.user .avatar { flex: 0 0 28px; width: 28px; height: 28px; border-radius: var(--radius-pill);
  background: var(--primary); color: var(--primary-text); display: flex; align-items: center;
  justify-content: center; }
.user .avatar svg { width: 17px; height: 17px; }
.user .who { display: flex; flex-direction: column; min-width: 0; line-height: 1.2; }
.user .who .name { font-weight: 600; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.user .who .role { font-size: 11px; color: var(--text-soft); white-space: nowrap; }
/* Collapsed state: icons only. Toggled by the logo, no JavaScript. */
.nav-collapse:checked ~ .layout .sidebar { width: var(--side-w-collapsed); flex-basis: var(--side-w-collapsed); }
.nav-collapse:checked ~ .layout .sidebar .brand-name,
.nav-collapse:checked ~ .layout .sidebar .label,
.nav-collapse:checked ~ .layout .sidebar .who { display: none; }
.nav-collapse:checked ~ .layout .sidebar .brand,
.nav-collapse:checked ~ .layout .sidebar .nav a,
.nav-collapse:checked ~ .layout .sidebar .user { justify-content: center; padding: 0; }
.main { flex: 1; min-width: 0; padding: 1.5rem 2rem; max-width: 920px; }
h1 { font-size: 22px; font-weight: 600; margin: 0 0 0.25rem; }
h2 { font-size: 16px; font-weight: 600; margin: 1.5rem 0 0.5rem; }
.sub { color: var(--text-soft); margin: 0 0 1.25rem; }
.card { background: var(--surface); border-radius: var(--radius-card);
  box-shadow: 0 0 0 1px rgba(26,20,22,0.06); padding: 1.1rem 1.2rem; margin-bottom: 1.1rem; }
label { display: block; margin-top: 0.7rem; font-weight: 500; }
input[type=text], input[type=password], textarea, select {
  width: 100%; margin-top: 0.25rem; padding: 0.5rem 0.65rem; font: inherit; color: var(--text);
  background: var(--surface); border: 1px solid var(--input); border-radius: var(--radius); }
textarea { min-height: 7rem; resize: vertical; }
input:focus, textarea:focus, select:focus { outline: none; border-color: #b8b4ae;
  box-shadow: 0 0 0 3px rgba(112,108,102,0.18); }
.btn { display: inline-flex; align-items: center; gap: 0.4rem; height: 36px; padding: 0 1rem;
  border: 1px solid transparent; border-radius: var(--radius-pill); background: var(--primary);
  color: var(--primary-text); font: inherit; font-weight: 500; cursor: pointer; text-decoration: none; }
.btn:hover { background: #45393c; }
.btn.secondary { background: var(--surface); border-color: var(--border); color: var(--text); }
.btn.secondary:hover { background: var(--muted); }
.btn.spotify { background: var(--accent); color: #08130b; }
.btn.spotify:hover { background: #1ed760; }
table { width: 100%; border-collapse: collapse; font-size: 14px; }
th { text-align: left; font-weight: 500; color: var(--text-soft); padding: 0.5rem 0.6rem;
  border-bottom: 1px solid var(--border); }
td { padding: 0.45rem 0.6rem; border-bottom: 1px solid rgba(234,232,229,0.6); }
tr:hover td { background: rgba(244,243,241,0.7); }
.banner { padding: 0.6rem 0.8rem; border-radius: var(--radius); background: var(--muted);
  margin: 0 0 0.8rem; }
.banner.error { background: var(--error-bg); color: var(--error-text); }
.pill { display: inline-flex; align-items: center; height: 22px; padding: 0 0.6rem;
  border-radius: var(--radius-pill); font-size: 12px; font-weight: 500;
  background: var(--muted-2); color: var(--text-soft); }
.pill.ok { background: #e4f4ea; color: var(--ok); }
.pill.warn { background: var(--error-bg); color: var(--error-text); }
ol.steps { padding-left: 0; counter-reset: step; list-style: none; margin: 0; }
ol.steps > li { position: relative; padding: 0 0 1rem 2.4rem; counter-increment: step; }
ol.steps > li::before { content: counter(step); position: absolute; left: 0; top: -2px;
  width: 1.6rem; height: 1.6rem; border-radius: var(--radius-pill); background: var(--primary);
  color: var(--primary-text); display: flex; align-items: center; justify-content: center;
  font-size: 12px; font-weight: 600; }
code { background: var(--muted-2); padding: 0.1rem 0.35rem; border-radius: 5px; font-size: 13px; }
.muted { color: var(--text-soft); }
pre.log { background: var(--muted); padding: 0.6rem 0.8rem; border-radius: var(--radius);
  max-height: 14rem; overflow-y: auto; font-size: 12.5px; }
/* dense, both-axis scrollable results table with a sticky header */
.results-scroll { overflow: auto; max-height: 62vh; border: 1px solid var(--border);
  border-radius: var(--radius); }
table.results-table { border-collapse: collapse; width: max-content; min-width: 100%;
  font-size: 12.5px; white-space: nowrap; }
table.results-table th, table.results-table td { padding: 4px 10px; text-align: left;
  border-bottom: 1px solid var(--border); }
table.results-table thead th { position: sticky; top: 0; z-index: 1; background: var(--surface);
  font-weight: 600; color: var(--text-soft); }
table.results-table tbody tr:hover { background: var(--muted); }
table.results-table .num { text-align: right; font-variant-numeric: tabular-nums; }
table.results-table .col-file { max-width: 42ch; overflow: hidden; text-overflow: ellipsis; }
table.results-table .col-folder { max-width: 28ch; overflow: hidden; text-overflow: ellipsis; }
/* clickable, sortable column headers */
table.results-table thead a.sort { cursor: pointer; color: inherit; text-decoration: none;
  user-select: none; white-space: nowrap; }
table.results-table thead a.sort:hover { color: var(--text); }
table.results-table thead th.sorted { color: var(--text); }
.btn.xs { height: 24px; padding: 0 0.65rem; font-size: 12px; }
.col-bar .bitrate-filter { margin: 0; }
.col-bar .bitrate-filter input { width: 5.5rem; }
/* CSS-only column show/hide. The .col-bar toggles sit on the page, outside the
   polled results fragment, so a 2s refresh never resets the chosen columns. */
.col-bar { display: flex; flex-wrap: wrap; align-items: center; gap: 0.2rem 0.85rem;
  margin: 0 0 0.6rem; font-size: 12.5px; color: var(--text-soft); }
.col-bar-label { font-weight: 600; }
.col-bar label { display: inline-flex; align-items: center; gap: 0.3rem; margin: 0; }
.col-bar:has(.ct-user:not(:checked)) ~ .results .col-user { display: none; }
.col-bar:has(.ct-folder:not(:checked)) ~ .results .col-folder { display: none; }
.col-bar:has(.ct-size:not(:checked)) ~ .results .col-size { display: none; }
.col-bar:has(.ct-bitrate:not(:checked)) ~ .results .col-bitrate { display: none; }
.col-bar:has(.ct-length:not(:checked)) ~ .results .col-length { display: none; }
.col-bar:has(.ct-slot:not(:checked)) ~ .results .col-slot { display: none; }
.col-bar:has(.ct-speed:not(:checked)) ~ .results .col-speed { display: none; }
.col-bar:has(.ct-queue:not(:checked)) ~ .results .col-queue { display: none; }
@media (max-width: 640px) {
  .layout { flex-direction: column; }
  .sidebar { width: auto !important; flex: none !important; height: auto; position: static;
    border-right: none; border-bottom: 1px solid var(--border); }
  .nav { flex-direction: row; flex-wrap: wrap; }
  .nav-footer { display: none; }
  .main { padding: 1rem; }
}
"#;

// Inline icons (20×20, stroke = currentColor) so no icon font/asset is needed.
const ICON_LOGO: &str = r##"<svg viewBox="0 0 26 26" fill="none"><rect x="1" y="1" width="24" height="24" rx="7" fill="currentColor"/><path d="M9.5 7v12" stroke="#fbfafa" stroke-width="1.8" stroke-linecap="round"/><path d="M14 10.5c0-1.2 1-2 2.4-2 1 0 1.8.35 2.3.9M19 15.5c0 1.3-1.1 2.1-2.6 2.1-1.1 0-1.9-.4-2.4-1" stroke="#fbfafa" stroke-width="1.7" stroke-linecap="round"/></svg>"##;
const ICON_SEARCH: &str = r#"<svg viewBox="0 0 20 20" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"><circle cx="9" cy="9" r="6"/><path d="M13.5 13.5L18 18"/></svg>"#;
const ICON_BULK: &str = r#"<svg viewBox="0 0 20 20" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><path d="M10 3v9"/><path d="M6 8.5l4 4 4-4"/><path d="M4 16.5h12"/></svg>"#;
const ICON_SPOTIFY: &str = r#"<svg viewBox="0 0 20 20" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"><circle cx="10" cy="10" r="8"/><path d="M5.8 8.2c3-1 6-.7 8.6.9"/><path d="M6.4 10.8c2.4-.7 4.8-.4 6.8 1"/><path d="M6.9 13.1c1.8-.5 3.5-.3 5 .8"/></svg>"#;
const ICON_SETTINGS: &str = r#"<svg viewBox="0 0 20 20" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"><path d="M3 6h7"/><path d="M14 6h3"/><circle cx="12" cy="6" r="2"/><path d="M3 14h3"/><path d="M10 14h7"/><circle cx="8" cy="14" r="2"/></svg>"#;
const ICON_USER: &str = r#"<svg viewBox="0 0 20 20" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"><circle cx="10" cy="7" r="3.2"/><path d="M4.6 16.4c.9-2.9 2.9-4.3 5.4-4.3s4.5 1.4 5.4 4.3"/></svg>"#;

fn escape_attr(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Wrap page `body` in the full document with the collapsible sidebar. `active`
/// is the nav key (`search`, `bulk`, `spotify`, `config`) of the current page;
/// `user` is the configured Soulseek username (empty when none) shown in the
/// footer account chip.
/// The results toolbar: a min-bitrate filter plus CSS-only column-visibility
/// toggles. It lives on the page (a sibling just before the polled results
/// container) so its state — and the typed filter value — survive the 2s
/// refresh. `min_bitrate` prefills the filter input (0 = no filter). The form
/// targets `next .results`, so the same toolbar works on the search and bulk
/// pages (each has its own `.results` div).
pub fn col_bar(min_bitrate: u32) -> String {
    let toggle = |id: &str, label: &str| {
        format!(r#"<label><input type="checkbox" class="ct-{id}" checked> {label}</label>"#)
    };
    format!(
        r##"<div class="col-bar"><form class="bitrate-filter" hx-post="/filter" hx-target="next .results" hx-swap="innerHTML" hx-trigger="change, submit"><label>min bitrate <input type="number" name="min_bitrate" value="{min}" min="0" step="32"> kbps</label></form><span class="col-bar-label">Columns</span>{user}{folder}{size}{bitrate}{length}{slot}{speed}{queue}</div>"##,
        min = min_bitrate,
        user = toggle("user", "User"),
        folder = toggle("folder", "Folder"),
        size = toggle("size", "Size"),
        bitrate = toggle("bitrate", "Bitrate"),
        length = toggle("length", "Length"),
        slot = toggle("slot", "Slot"),
        speed = toggle("speed", "Speed"),
        queue = toggle("queue", "Queue"),
    )
}

pub fn shell(title: &str, active: &str, user: &str, body: &str) -> String {
    let nav = |key: &str, href: &str, label: &str, icon: &str| {
        let class = if key == active { "active" } else { "" };
        format!(
            r#"<a class="{class}" href="{href}"><span class="ico">{icon}</span><span class="label">{label}</span></a>"#
        )
    };
    let (name, role) = if user.trim().is_empty() {
        ("not signed in".to_string(), "click to sign in")
    } else {
        (escape_attr(user.trim()), "Soulseek account")
    };
    format!(
        r#"<!DOCTYPE html>
<html lang="en"><head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<script src="/assets/htmx.min.js"></script>
<script src="/assets/app.js" defer></script>
<style>{css}</style>
</head><body>
<input type="checkbox" id="nav-collapse" class="nav-collapse">
<div class="layout">
<aside class="sidebar">
  <label class="brand" for="nav-collapse" title="Collapse / expand the sidebar">
    <span class="logo">{logo}</span><span class="brand-name">soulrust</span>
  </label>
  <nav class="nav">
    {search}
    {bulk}
    {spotify}
    {config}
  </nav>
  <div class="nav-footer">
    <a class="user" href="/account" title="{name} — sign in / account">
      <span class="avatar">{user_icon}</span>
      <span class="who"><span class="name">{name}</span><span class="role">{role}</span></span>
    </a>
  </div>
</aside>
<main class="main">
{body}
</main>
</div>
</body></html>"#,
        title = escape_attr(title),
        css = THEME_CSS,
        logo = ICON_LOGO,
        search = nav("search", "/", "Search", ICON_SEARCH),
        bulk = nav("bulk", "/bulk", "Bulk downloads", ICON_BULK),
        spotify = nav("spotify", "/spotify", "Spotify", ICON_SPOTIFY),
        config = nav("config", "/config", "Settings", ICON_SETTINGS),
        user_icon = ICON_USER,
        name = name,
        role = role,
        body = body,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_has_css_only_collapse_icons_and_active_nav() {
        let html = shell("t", "bulk", "alice", "<p>hi</p>");
        // CSS-only collapse: a checkbox toggled by the logo label, with rules
        // that shrink the sidebar (no JS for the collapse itself).
        assert!(html.contains(r#"id="nav-collapse""#));
        assert!(html.contains(r#"<label class="brand" for="nav-collapse""#));
        assert!(html.contains(".nav-collapse:checked ~ .layout .sidebar"));
        // Every nav item carries an icon (inline svg) and a label.
        assert!(html.matches("<span class=\"ico\">").count() >= 4);
        assert!(html.contains("<svg"));
        // The current page is marked active.
        assert!(html.contains(r#"<a class="active" href="/bulk">"#));
        assert!(html.contains("<p>hi</p>"));
    }

    #[test]
    fn footer_account_chip_reflects_the_user() {
        let signed_in = shell("t", "search", "alice", "");
        assert!(signed_in.contains(r#"class="user""#));
        assert!(signed_in.contains("alice"));
        let none = shell("t", "search", "  ", "");
        assert!(none.contains("not signed in"));
    }
}
