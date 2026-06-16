//! Matching an incoming search against our shares, mirroring Nicotine+'s
//! `search.py` (`_create_search_result_list` / `_create_file_info_list`). Pure:
//! it operates on the in-memory [`crate::shares::ShareIndex`] and a word index,
//! so it's fully unit-testable.

use std::collections::{HashMap, HashSet};

use soulseek_proto::peer_message::SharedFile;

use crate::shares::ShareIndex;

/// A parsed search query: plain (included) words, `-`excluded words, and
/// `*`partial (suffix) words.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchTerms {
    pub included: Vec<String>,
    pub excluded: Vec<String>,
    pub partial: Vec<String>,
}

/// Splits a raw search term into included / excluded(`-`) / partial(`*`) words,
/// lowercased. (Nicotine+ does additional quote/punctuation stripping; this is
/// the core classification the matcher needs.)
pub fn sanitize_search_term(term: &str) -> SearchTerms {
    let mut terms = SearchTerms::default();
    for raw in term.split_whitespace() {
        if let Some(word) = raw.strip_prefix('-') {
            if !word.is_empty() {
                terms.excluded.push(word.to_lowercase());
            }
        } else if let Some(word) = raw.strip_prefix('*') {
            if !word.is_empty() {
                terms.partial.push(word.to_lowercase());
            }
        } else if !raw.is_empty() {
            terms.included.push(raw.to_lowercase());
        }
    }
    terms
}

/// Intersects the word index to the set of file ids matching the query, or
/// `None` if nothing matches (or there are no usable included words) — the
/// algorithm from `search.py:_create_search_result_list`.
pub fn create_search_result_list(
    terms: &SearchTerms,
    max_results: usize,
    word_index: &HashMap<String, Vec<u32>>,
) -> Option<HashSet<u32>> {
    // Require at least one complete included word, as official clients do.
    let mut included = terms.included.iter();
    let first = included.next()?;
    let first_ids = word_index.get(first)?;

    // Single included word with no partials: truncate to the result cap early.
    let mut results: HashSet<u32> =
        if terms.included.len() == 1 && terms.partial.is_empty() {
            first_ids.iter().take(max_results).copied().collect()
        } else {
            first_ids.iter().copied().collect()
        };
    if results.is_empty() {
        return None;
    }

    for word in included {
        let ids: HashSet<u32> = word_index.get(word)?.iter().copied().collect();
        results.retain(|id| ids.contains(id));
        if results.is_empty() {
            return None;
        }
    }

    for partial in &terms.partial {
        let mut partial_results = HashSet::new();
        for (word, ids) in word_index {
            if word.ends_with(partial.as_str()) {
                partial_results.extend(ids.iter().copied().filter(|id| results.contains(id)));
            }
        }
        if partial_results.is_empty() {
            return None;
        }
        results.retain(|id| partial_results.contains(id));
        if results.is_empty() {
            return None;
        }
    }

    for excluded in &terms.excluded {
        if let Some(ids) = word_index.get(excluded) {
            for id in ids {
                results.remove(id);
            }
            if results.is_empty() {
                return None;
            }
        }
    }

    Some(results)
}

/// Turns matched file ids into the wire `SharedFile` list, capped at
/// `max_results` and dropping any path containing a server-excluded phrase
/// (`search.py:_create_file_info_list`). Results are ordered by virtual path
/// for determinism.
pub fn create_file_info_list(
    results: &HashSet<u32>,
    max_results: usize,
    excluded_phrases: &[String],
    index: &ShareIndex,
) -> Vec<SharedFile> {
    let phrases: Vec<String> = excluded_phrases.iter().map(|p| p.to_lowercase()).collect();
    let mut ids: Vec<u32> = results.iter().copied().collect();
    ids.sort_by(|&a, &b| index.files[a as usize].virtual_path.cmp(&index.files[b as usize].virtual_path));

    let mut out = Vec::new();
    for id in ids {
        if out.len() >= max_results {
            break;
        }
        let lower = index.files[id as usize].virtual_path.to_lowercase();
        if phrases.iter().any(|p| lower.contains(p.as_str())) {
            continue;
        }
        out.push(index.shared_file(id));
    }
    out
}

/// Convenience: match `term` against `index` and return the files to put in a
/// FileSearchResponse, or an empty vec if nothing matches.
pub fn respond(
    term: &str,
    max_results: usize,
    excluded_phrases: &[String],
    index: &ShareIndex,
) -> Vec<SharedFile> {
    let terms = sanitize_search_term(term);
    match create_search_result_list(&terms, max_results, &index.word_index) {
        Some(results) => create_file_info_list(&results, max_results, excluded_phrases, index),
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word_index() -> HashMap<String, Vec<u32>> {
        // The exact fixture from Nicotine+'s test_create_search_result_list.
        HashMap::from([
            ("iso".into(), vec![34, 35, 36, 37, 38]),
            ("lts".into(), vec![63, 68, 73]),
            ("system".into(), vec![37, 38]),
            ("linux".into(), vec![35, 36]),
        ])
    }

    fn terms(included: &[&str], excluded: &[&str], partial: &[&str]) -> SearchTerms {
        SearchTerms {
            included: included.iter().map(|s| s.to_string()).collect(),
            excluded: excluded.iter().map(|s| s.to_string()).collect(),
            partial: partial.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn included_partial_excluded_intersection() {
        // Ported from test_search.py::test_create_search_result_list.
        let wi = word_index();
        assert_eq!(
            create_search_result_list(&terms(&["iso"], &["linux", "game"], &["stem"]), 1500, &wi),
            Some(HashSet::from([37, 38]))
        );
        // Disjoint included words → no match.
        assert_eq!(
            create_search_result_list(&terms(&["lts", "iso"], &["linux", "game", "music", "cd"], &[]), 1500, &wi),
            None
        );
        // Partial word matches nothing → no match.
        assert_eq!(
            create_search_result_list(&terms(&["iso"], &["system"], &["ibberish"]), 1500, &wi),
            None
        );
    }

    #[test]
    fn sanitize_classifies_prefixes() {
        let t = sanitize_search_term("Gwen -mp3 *ello -No yes");
        assert_eq!(t.included, vec!["gwen", "yes"]);
        assert_eq!(t.excluded, vec!["mp3", "no"]);
        assert_eq!(t.partial, vec!["ello"]);
    }

    fn index_with(paths: &[(&str, u64)]) -> ShareIndex {
        let mut index = ShareIndex::default();
        for (i, (path, size)) in paths.iter().enumerate() {
            let _ = i;
            // Build the index by hand (bypassing the filesystem) for matcher tests.
            index.files.push(crate::shares::SharedFileEntry {
                real_path: path.into(),
                virtual_path: (*path).into(),
                size: *size,
            });
            let id = (index.files.len() - 1) as u32;
            for token in crate::shares::tokenize(path) {
                index.word_index.entry(token).or_default().push(id);
            }
        }
        index
    }

    #[test]
    fn excluded_phrases_drop_matching_files() {
        // Ported from test_search.py::test_exclude_server_phrases.
        let index = index_with(&[
            ("isos\\freebsd.iso", 1000),
            ("isos\\linux.iso", 2000),
            ("isos\\linux distro.iso", 3000),
            ("isos\\NetBSD.iso", 5000),
            ("isos\\openbsd.iso", 6000),
        ]);
        let results: HashSet<u32> = (0..index.num_files() as u32).collect();
        let phrases = vec!["linux distro".to_string(), "netbsd".to_string()];
        let files = create_file_info_list(&results, 100, &phrases, &index);

        let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"isos\\linux.iso"));
        assert!(names.contains(&"isos\\freebsd.iso"));
        assert!(names.contains(&"isos\\openbsd.iso"));
        assert!(!names.iter().any(|n| n.contains("distro") || n.to_lowercase().contains("netbsd")));
        assert_eq!(files.len(), 3);
    }

    #[test]
    fn respond_matches_against_a_real_index() {
        let index = index_with(&[("Music\\Gwen\\song.mp3", 10), ("Music\\Other\\clip.flac", 20)]);
        let hits = respond("gwen", 100, &[], &index);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "Music\\Gwen\\song.mp3");
        // No included word in the index → no results, not an error.
        assert!(respond("nonexistentword", 100, &[], &index).is_empty());
    }
}
