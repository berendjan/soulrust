/// Sanitize a string into a single, safe filesystem path component: strip any
/// path separators, drop `..`/`.`-only names and leading dots, collapse control
/// characters and characters illegal on common filesystems, and trim. Returns
/// an empty string if nothing usable remains — callers treat empty as "none".
/// Used for the playlist subfolder and track-number prefix in the "organize"
/// download option, so a playlist title can never escape the download dir.
pub(crate) fn sanitize_path_component(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        match c {
            // Path separators and characters illegal on Windows/macOS filesystems.
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => out.push('_'),
            c if (c as u32) < 0x20 => {} // control chars: drop
            c => out.push(c),
        }
    }
    // Trim surrounding whitespace and dots (a name that is all dots — "." or
    // ".." — or has a trailing dot is invalid or confusing on some filesystems).
    let trimmed = out.trim().trim_matches('.').trim();
    trimmed.to_owned()
}

pub mod api_server;
pub mod browse;
pub mod github;
pub mod net_edge;
pub mod peer_net;
pub mod session;
pub mod transfer_io;
pub mod ui;
pub mod ui_theme;
pub mod updater;
pub mod web_bridge;

#[cfg(test)]
mod tests {
    use super::sanitize_path_component;

    #[test]
    fn sanitize_keeps_safe_names_and_neutralizes_dangerous_ones() {
        // A plain title is kept verbatim.
        assert_eq!(sanitize_path_component("Road Trip 2024"), "Road Trip 2024");
        // Path separators and filesystem-illegal characters become underscores,
        // so a title can never span directories or escape the download dir.
        assert_eq!(sanitize_path_component("AC/DC: Live"), "AC_DC_ Live");
        assert_eq!(sanitize_path_component("../etc/passwd"), "_etc_passwd");
        assert_eq!(sanitize_path_component("a\\b"), "a_b");
        // Surrounding whitespace and dots are trimmed; a dots-only name is empty.
        assert_eq!(sanitize_path_component("  hi.  "), "hi");
        assert_eq!(sanitize_path_component(".."), "");
        assert_eq!(sanitize_path_component(""), "");
        // Control characters are dropped.
        assert_eq!(sanitize_path_component("mix\t\n1"), "mix1");
    }
}
