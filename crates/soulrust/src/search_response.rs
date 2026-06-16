//! Matching an incoming search against our shares, mirroring Nicotine+'s
//! `search.py` (`_create_search_result_list` / `_create_file_info_list`). Pure:
//! it operates on the in-memory [`crate::shares::ShareIndex`] and a word index,
//! so it's fully unit-testable.

use std::collections::{HashMap, HashSet};

use soulseek_proto::peer_message::{FileSearchResponse, SharedFile};

use crate::shares::ShareIndex;

/// Requester-side filtering of inbound search results, mirroring Soulseek.NET's
/// `SearchOptions` (minimum file count, minimum peer upload speed, maximum peer
/// queue length). A response that fails any criterion is dropped before it ever
/// reaches the UI, so a flood of empty/slow/backed-up peers can't bury the good
/// hits. Pure, so the thresholds are exhaustively testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchFilter {
    /// Minimum number of files a response must carry (treated as at least 1, so
    /// an empty response is always dropped).
    pub min_files: u32,
    /// Minimum advertised upload speed (0 = no minimum).
    pub min_upload_speed: u32,
    /// Maximum advertised queue length (0 = no limit).
    pub max_queue_length: u32,
}

impl SearchFilter {
    /// Whether an inbound response clears every configured threshold.
    pub fn accepts(&self, response: &FileSearchResponse) -> bool {
        (response.files.len() as u32) >= self.min_files.max(1)
            && response.upload_speed >= self.min_upload_speed
            && (self.max_queue_length == 0 || response.in_queue <= self.max_queue_length)
    }
}

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

    // Truncate to the result cap early only when the whole query is a single
    // word. Nicotine+ gates this on `len(included)+len(excluded)+len(partial)
    // == 1` (search.py:_create_search_result_list `has_single_word`), so an
    // excluded or partial word must keep the full `start_results` — otherwise
    // exclusion/intersection would run against a prematurely-truncated set and
    // drop matches the reference keeps.
    let single_word =
        terms.included.len() == 1 && terms.partial.is_empty() && terms.excluded.is_empty();
    let mut results: HashSet<u32> = if single_word {
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

    if !terms.partial.is_empty() {
        // One pass over the vocabulary, bucketing matches per partial term,
        // instead of re-scanning the whole word index once per partial.
        let mut matched: Vec<HashSet<u32>> = vec![HashSet::new(); terms.partial.len()];
        for (word, ids) in word_index {
            for (bucket, partial) in matched.iter_mut().zip(&terms.partial) {
                if word.ends_with(partial.as_str()) {
                    bucket.extend(ids.iter().copied().filter(|id| results.contains(id)));
                }
            }
        }
        for partial_results in &matched {
            if partial_results.is_empty() {
                return None;
            }
            results.retain(|id| partial_results.contains(id));
            if results.is_empty() {
                return None;
            }
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
    let path = |id: u32| index.files[id as usize].virtual_path.as_str();
    let mut ids: Vec<u32> = results.iter().copied().collect();

    // Keep only the `max_results` smallest-by-path ids via a partial sort
    // (O(n) select + sort of the kept k), rather than fully sorting all matches
    // just to discard most of them. Nicotine+ caps the candidate set to
    // `max_results` *before* dropping excluded-phrase files (islice then
    // _append_file_info), so the response may carry fewer than `max_results`.
    if ids.len() > max_results {
        ids.select_nth_unstable_by(max_results, |&a, &b| path(a).cmp(path(b)));
        ids.truncate(max_results);
    }
    ids.sort_by(|&a, &b| path(a).cmp(path(b)));

    if excluded_phrases.is_empty() {
        return ids.into_iter().map(|id| index.shared_file(id)).collect();
    }
    let phrases: Vec<String> = excluded_phrases.iter().map(|p| p.to_lowercase()).collect();
    ids.into_iter()
        .filter(|&id| {
            let lower = path(id).to_lowercase();
            !phrases.iter().any(|p| lower.contains(p.as_str()))
        })
        .map(|id| index.shared_file(id))
        .collect()
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
    fn single_included_word_truncates_only_without_excluded_or_partial() {
        // Nicotine+ truncates `start_results[:max_results]` only when the whole
        // query is a single word: `has_single_word = len(included)+len(excluded)
        // +len(partial) == 1` (search.py:_create_search_result_list).
        let wi = word_index();

        // One included word, no excluded/partial → single-word path: the
        // start results ARE truncated to max_results (first two of iso's ids).
        assert_eq!(
            create_search_result_list(&terms(&["iso"], &[], &[]), 2, &wi),
            Some(HashSet::from([34, 35]))
        );

        // One included word *with* an excluded word is NOT a single-word query,
        // so the full `iso` list survives to the exclusion step. With max=2 the
        // reference keeps {34,35,36,37,38}, then removes linux={35,36} → leaving
        // {34,37,38}. (A premature truncate-to-2 would wrongly yield just {34}.)
        assert_eq!(
            create_search_result_list(&terms(&["iso"], &["linux"], &[]), 2, &wi),
            Some(HashSet::from([34, 37, 38]))
        );
    }

    #[test]
    fn no_included_word_returns_none() {
        // Nicotine+ requires at least one complete included word: `start_word =
        // next(iter(included_words), None); if not start_word: return None`
        // (search.py:_create_search_result_list). A query of only excluded
        // and/or partial words yields nothing.
        let wi = word_index();
        assert_eq!(create_search_result_list(&terms(&[], &["linux"], &["stem"]), 1500, &wi), None);
        assert_eq!(create_search_result_list(&terms(&[], &[], &["stem"]), 1500, &wi), None);
    }

    #[test]
    fn sanitize_classifies_prefixes() {
        let t = sanitize_search_term("Gwen -mp3 *ello -No yes");
        assert_eq!(t.included, vec!["gwen", "yes"]);
        assert_eq!(t.excluded, vec!["mp3", "no"]);
        assert_eq!(t.partial, vec!["ello"]);
    }

    #[test]
    fn sanitize_handles_edge_cases() {
        // Empty / whitespace-only -> nothing in any bucket.
        let empty = sanitize_search_term("   \t  ");
        assert!(empty.included.is_empty() && empty.excluded.is_empty() && empty.partial.is_empty());

        // A bare prefix with no word is dropped (no empty-string entries).
        let bare = sanitize_search_term("- * foo");
        assert_eq!(bare.included, vec!["foo"]);
        assert!(bare.excluded.is_empty(), "bare '-' adds nothing");
        assert!(bare.partial.is_empty(), "bare '*' adds nothing");
        // '--foo' strips only the leading '-', leaving '-foo' as an excluded word.
        assert_eq!(sanitize_search_term("--foo").excluded, vec!["-foo"]);

        // Repeated whitespace collapses; case is normalized.
        let spaced = sanitize_search_term("  AAA   -BBB  ");
        assert_eq!(spaced.included, vec!["aaa"]);
        assert_eq!(spaced.excluded, vec!["bbb"]);

        // Only the leading char classifies: a '-' or '*' mid-word stays included.
        let mid = sanitize_search_term("a-b c*d");
        assert_eq!(mid.included, vec!["a-b", "c*d"]);
    }

    fn index_with(paths: &[(&str, u64)]) -> ShareIndex {
        // Use the real indexer (add_virtual) rather than hand-rolling indexing,
        // so the test matches the index shape a real scan produces (same word
        // dedup and folder mapping).
        let mut index = ShareIndex::default();
        for (path, size) in paths {
            index.add_virtual(path, *size);
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
    fn excluded_phrase_filter_runs_after_result_cap() {
        // Nicotine+ caps the candidate set to max_results *first* —
        // `islice(results, min(len(results), max_results))` — and only then
        // drops excluded-phrase files in `_append_file_info`. So an excluded
        // file among the capped candidates shrinks the response below
        // max_results; the matcher must not reach past the cap to backfill.
        let index = index_with(&[
            ("a_bad.iso", 1000), // sorts first, hits the "bad" phrase
            ("b.iso", 2000),
            ("c.iso", 3000),
            ("d.iso", 4000),
        ]);
        let results: HashSet<u32> = (0..index.num_files() as u32).collect();
        let phrases = vec!["bad".to_string()];

        // max_results=2 caps to the first two sorted paths {a_bad.iso, b.iso};
        // a_bad.iso is excluded, so exactly one file (b.iso) is returned — NOT
        // backfilled to two with c.iso.
        let files = create_file_info_list(&results, 2, &phrases, &index);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "b.iso");
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

    #[test]
    fn multiple_included_words_intersect() {
        // All included words must be present (a set intersection), like
        // Nicotine+'s _create_search_result_list.
        let index = index_with(&[
            ("Music\\Gwen\\Stefani\\hit.mp3", 1),
            ("Music\\Gwen\\Other\\clip.mp3", 2),
        ]);
        // Both files contain "gwen", only the first also contains "stefani".
        assert_eq!(respond("gwen", 100, &[], &index).len(), 2);
        let hits = respond("gwen stefani", 100, &[], &index);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "Music\\Gwen\\Stefani\\hit.mp3");
        // A second word absent from every file yields no results.
        assert!(respond("gwen nonexistent", 100, &[], &index).is_empty());
    }

    fn response_with(files: usize, speed: u32, queue: u32) -> FileSearchResponse {
        FileSearchResponse {
            username: "peer".into(),
            token: 1,
            files: (0..files).map(|i| SharedFile { name: format!("f{i}"), ..Default::default() }).collect(),
            free_slots: true,
            upload_speed: speed,
            in_queue: queue,
            private_files: Vec::new(),
        }
    }

    #[test]
    fn search_filter_enforces_thresholds() {
        // No constraints (min_files coerced to 1): any non-empty response passes,
        // an empty one never does.
        let open = SearchFilter { min_files: 0, min_upload_speed: 0, max_queue_length: 0 };
        assert!(open.accepts(&response_with(1, 0, 9_999)));
        assert!(!open.accepts(&response_with(0, 0, 0)), "empty response always dropped");

        // Minimum file count.
        let min_files = SearchFilter { min_files: 3, min_upload_speed: 0, max_queue_length: 0 };
        assert!(!min_files.accepts(&response_with(2, 0, 0)));
        assert!(min_files.accepts(&response_with(3, 0, 0)));

        // Minimum upload speed.
        let min_speed = SearchFilter { min_files: 1, min_upload_speed: 100, max_queue_length: 0 };
        assert!(!min_speed.accepts(&response_with(1, 99, 0)));
        assert!(min_speed.accepts(&response_with(1, 100, 0)));

        // Maximum queue length (0 = unlimited; otherwise inclusive).
        let max_queue = SearchFilter { min_files: 1, min_upload_speed: 0, max_queue_length: 5 };
        assert!(max_queue.accepts(&response_with(1, 0, 5)));
        assert!(!max_queue.accepts(&response_with(1, 0, 6)));
    }

    #[test]
    fn partial_words_match_by_suffix() {
        // A `*term` partial matches any indexed word ending in `term`, narrowing
        // the included results (Nicotine+'s partial-word handling).
        let index = index_with(&[
            ("Music\\song.mp3", 1),
            ("Music\\song.flac", 2),
        ]);
        // "song" matches both; the partial "*mp3" keeps only the .mp3 (its
        // extension tokenizes to the word "mp3").
        let hits = respond("song *mp3", 100, &[], &index);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "Music\\song.mp3");
        // A partial that matches nothing drops all results.
        assert!(respond("song *xyz", 100, &[], &index).is_empty());
    }
}
