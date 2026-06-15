//! The browse read-model: accumulates the share listings fetched by the peer
//! edge (keyed by username) and renders the browse fragment the UI polls.
//!
//! Like [`crate::components::ui`] it is a pure view-state component — it only
//! consumes result/failure broadcasts and answers render requests, so it is
//! fully unit-testable with a capturing writer.

use std::collections::HashMap;

use rust_messenger::traits;
use rust_messenger::traits::extended::Sender;

use crate::config::AppContext;
use crate::messages::{
    BrowseFailed, BrowseHtml, BrowseListing, BrowseRenderReq, HandlerId,
};

enum Entry {
    Loaded(BrowseListing),
    Failed(String),
}

pub struct Browse {
    /// Per-user state, plus the order users were last updated (newest first).
    entries: HashMap<String, Entry>,
    order: Vec<String>,
}

impl Browse {
    pub fn new<W: traits::core::Writer>(_ctx: &AppContext, _writer: &W) -> Self {
        Browse { entries: HashMap::new(), order: Vec::new() }
    }

    fn touch(&mut self, username: &str) {
        self.order.retain(|u| u != username);
        self.order.insert(0, username.to_owned());
    }

    fn render(&self) -> String {
        if self.order.is_empty() {
            return "<p>no browses yet — enter a username above</p>".into();
        }
        let mut out = String::new();
        for username in &self.order {
            match self.entries.get(username) {
                Some(Entry::Failed(reason)) => {
                    out.push_str(&format!(
                        r#"<div class="banner error">browse of <b>{}</b> failed: {}</div>"#,
                        escape(username),
                        escape(reason)
                    ));
                }
                Some(Entry::Loaded(listing)) => out.push_str(&render_listing(listing)),
                None => {}
            }
        }
        out
    }
}

fn render_listing(listing: &BrowseListing) -> String {
    let partial = if listing.truncated { " (partial — share is larger)" } else { "" };
    let mut out = format!(
        r#"<details open><summary><b>{}</b> — {} file(s){}</summary>"#,
        escape(&listing.username),
        listing.total_files,
        partial
    );
    for dir in &listing.directories {
        out.push_str(&format!(r#"<div class="dir">{}</div><ul>"#, escape(&dir.path)));
        for file in &dir.files {
            out.push_str(&format!(
                "<li>{} <span class=\"size\">({})</span></li>",
                escape(&file.name),
                human_size(file.size)
            ));
        }
        out.push_str("</ul>");
    }
    out.push_str("</details>");
    out
}

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

impl traits::core::Handler for Browse {
    type Id = HandlerId;
    const ID: HandlerId = HandlerId::Browse;
}

impl traits::core::Handle<BrowseListing> for Browse {
    fn handle<W: traits::core::Writer>(&mut self, message: &BrowseListing, _writer: &W) {
        self.touch(&message.username);
        self.entries.insert(message.username.clone(), Entry::Loaded(message.clone()));
    }
}

impl traits::core::Handle<BrowseFailed> for Browse {
    fn handle<W: traits::core::Writer>(&mut self, message: &BrowseFailed, _writer: &W) {
        self.touch(&message.username);
        self.entries.insert(message.username.clone(), Entry::Failed(message.reason.clone()));
    }
}

impl traits::core::Handle<BrowseRenderReq> for Browse {
    fn handle<W: traits::core::Writer>(&mut self, message: &BrowseRenderReq, writer: &W) {
        Self::send(&BrowseHtml { corr: message.corr, html: self.render() }, writer);
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
    use crate::messages::{BrowseDir, BrowseFile, MessageId};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct CapturingWriter {
        records: Arc<Mutex<Vec<(u16, Vec<u8>)>>>,
    }

    impl traits::core::Writer for CapturingWriter {
        fn write<M: traits::core::Message, H: traits::core::Handler, F: FnOnce(&mut [u8])>(
            &self,
            size: usize,
            callback: F,
        ) {
            let mut buf = vec![0u8; size];
            callback(&mut buf);
            self.records.lock().unwrap().push((M::ID.into(), buf));
        }
    }

    impl CapturingWriter {
        fn rendered(&self) -> Vec<String> {
            self.records
                .lock()
                .unwrap()
                .iter()
                .filter(|(id, _)| *id == u16::from(MessageId::BrowseHtml))
                .map(|(_, buf)| BrowseHtml::deserialize_from(buf).html)
                .collect()
        }
    }

    fn browse() -> Browse {
        Browse::new(&AppContext::new(Default::default(), "/tmp/x.yaml".into()), &CapturingWriter::default())
    }

    fn listing(username: &str) -> BrowseListing {
        BrowseListing {
            username: username.into(),
            directories: vec![BrowseDir {
                path: "Music\\Album".into(),
                files: vec![BrowseFile { name: "song.mp3".into(), size: 5_242_880 }],
            }],
            total_files: 1,
            truncated: false,
        }
    }

    fn render(b: &mut Browse) -> String {
        let writer = CapturingWriter::default();
        traits::core::Handle::<BrowseRenderReq>::handle(b, &BrowseRenderReq { corr: 1 }, &writer);
        writer.rendered().pop().unwrap()
    }

    #[test]
    fn empty_state_prompts_for_a_username() {
        let mut b = browse();
        assert!(render(&mut b).contains("no browses yet"));
    }

    #[test]
    fn loaded_listing_shows_files_with_human_sizes() {
        let mut b = browse();
        traits::core::Handle::<BrowseListing>::handle(&mut b, &listing("alice"), &CapturingWriter::default());
        let html = render(&mut b);
        assert!(html.contains("<b>alice</b>"));
        assert!(html.contains("Music\\Album"));
        assert!(html.contains("song.mp3"));
        assert!(html.contains("5.0 MB"));
    }

    #[test]
    fn failure_renders_an_error_banner() {
        let mut b = browse();
        traits::core::Handle::<BrowseFailed>::handle(
            &mut b,
            &BrowseFailed { username: "bob".into(), reason: "offline".into() },
            &CapturingWriter::default(),
        );
        let html = render(&mut b);
        assert!(html.contains("browse of <b>bob</b> failed"));
        assert!(html.contains("offline"));
    }

    #[test]
    fn newest_browse_renders_first_and_replaces_prior_state() {
        let mut b = browse();
        traits::core::Handle::<BrowseListing>::handle(&mut b, &listing("alice"), &CapturingWriter::default());
        traits::core::Handle::<BrowseListing>::handle(&mut b, &listing("bob"), &CapturingWriter::default());
        let html = render(&mut b);
        assert!(html.find("bob").unwrap() < html.find("alice").unwrap());

        // A later failure for alice replaces her loaded listing.
        traits::core::Handle::<BrowseFailed>::handle(
            &mut b,
            &BrowseFailed { username: "alice".into(), reason: "lost connection".into() },
            &CapturingWriter::default(),
        );
        let html = render(&mut b);
        assert!(html.contains("browse of <b>alice</b> failed"));
        assert!(html.find("alice").unwrap() < html.find("bob").unwrap());
    }

    #[test]
    fn truncated_listing_is_marked_partial() {
        let mut b = browse();
        let mut l = listing("alice");
        l.truncated = true;
        l.total_files = 99999;
        traits::core::Handle::<BrowseListing>::handle(&mut b, &l, &CapturingWriter::default());
        assert!(render(&mut b).contains("partial"));
    }

    #[test]
    fn html_in_paths_and_names_is_escaped() {
        let mut b = browse();
        let mut l = listing("alice");
        l.directories[0].path = "<script>".into();
        traits::core::Handle::<BrowseListing>::handle(&mut b, &l, &CapturingWriter::default());
        let html = render(&mut b);
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
    }
}
