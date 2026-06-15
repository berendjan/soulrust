//! Fallback extractor: any non-empty input becomes a single verbatim search.

use crate::config::Config;
use crate::extract::{ExtractError, Extractor, Job, SearchJob};

pub struct PlainQueryExtractor;

impl Extractor for PlainQueryExtractor {
    fn matches(&self, input: &str) -> bool {
        !input.trim().is_empty()
    }

    fn extract(&self, input: &str, _config: &Config) -> Result<Job, ExtractError> {
        let query = input.trim().to_owned();
        Ok(Job {
            source_label: format!("search: {query}"),
            searches: vec![SearchJob { raw_query: Some(query), ..Default::default() }],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_input_becomes_one_verbatim_search() {
        let job = PlainQueryExtractor
            .extract("  artist - title  ", &Config::default())
            .unwrap();
        assert_eq!(job.searches.len(), 1);
        assert_eq!(job.searches[0].to_query(), "artist - title");
    }
}
