//! Shared look-and-feel for the web UI: the page shell (sidebar nav + header)
//! and the CSS theme, so every page — the search dashboard, bulk downloads,
//! Spotify setup, and config — renders consistently. The styling mirrors the
//! Rootline `dashboard-beta` design: a light, cool-neutral palette, the Inter
//! type stack, generous rounding, and subtle ring borders instead of shadows.

/// The full stylesheet, embedded in every page `<head>`.
pub const THEME_CSS: &str = r#"
:root {
  --bg: #fcfbfa; --surface: #ffffff; --muted: #f4f3f1; --muted-2: #efedea;
  --text: #1a1416; --text-soft: #6b6560; --border: #eae8e5; --input: #e2e0dc;
  --primary: #2d2729; --primary-text: #fbfafa; --accent: #1db954;
  --error-bg: #fbeae8; --error-text: #a23a2e; --ok: #1f7a4d;
  --radius: 10px; --radius-pill: 999px; --radius-card: 18px;
}
* { box-sizing: border-box; }
body {
  margin: 0; background: var(--bg); color: var(--text);
  font-family: 'Inter', system-ui, -apple-system, 'Segoe UI', sans-serif;
  font-size: 14px; line-height: 1.5;
}
a { color: inherit; }
.layout { display: flex; min-height: 100vh; }
.sidebar {
  width: 208px; flex: 0 0 208px; background: #f6f5f3;
  border-right: 1px solid var(--border); padding: 0.75rem; position: sticky; top: 0; height: 100vh;
}
.brand { display: flex; align-items: center; gap: 0.5rem; height: 40px; padding: 0 0.6rem;
  font-weight: 600; font-size: 15px; }
.brand .dot { width: 14px; height: 14px; border-radius: 4px; background: var(--primary); }
.nav { margin-top: 0.5rem; display: flex; flex-direction: column; gap: 2px; }
.nav a { height: 36px; display: flex; align-items: center; gap: 0.55rem; padding: 0 0.75rem;
  border-radius: var(--radius-pill); text-decoration: none; color: var(--text-soft); font-weight: 500; }
.nav a:hover { background: var(--muted-2); color: var(--text); }
.nav a.active { background: rgba(45,39,41,0.06); color: var(--text); }
.main { flex: 1; min-width: 0; padding: 1.5rem 2rem; max-width: 900px; }
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
@media (max-width: 640px) {
  .layout { flex-direction: column; }
  .sidebar { width: auto; flex: none; height: auto; position: static; border-right: none;
    border-bottom: 1px solid var(--border); }
  .nav { flex-direction: row; flex-wrap: wrap; }
  .main { padding: 1rem; }
}
"#;

/// Wrap page `body` in the full document with the sidebar nav. `active` is the
/// nav key (`search`, `bulk`, `spotify`, `config`) of the current page.
pub fn shell(title: &str, active: &str, body: &str) -> String {
    let nav = |key: &str, href: &str, label: &str| {
        let class = if key == active { "active" } else { "" };
        format!(r#"<a class="{class}" href="{href}">{label}</a>"#)
    };
    format!(
        r#"<!DOCTYPE html>
<html lang="en"><head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<script src="/assets/htmx.min.js"></script>
<style>{css}</style>
</head><body>
<div class="layout">
<aside class="sidebar">
  <div class="brand"><span class="dot"></span> soulrust</div>
  <nav class="nav">
    {search}
    {bulk}
    {spotify}
    {config}
  </nav>
</aside>
<main class="main">
{body}
</main>
</div>
</body></html>"#,
        title = title,
        css = THEME_CSS,
        search = nav("search", "/", "Search"),
        bulk = nav("bulk", "/bulk", "Bulk downloads"),
        spotify = nav("spotify", "/spotify", "Spotify"),
        config = nav("config", "/config", "Settings"),
        body = body,
    )
}
