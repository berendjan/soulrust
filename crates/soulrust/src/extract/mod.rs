//! Input extraction: turning whatever the user pastes into search jobs.
//!
//! Modeled on sockseek's `IExtractor`: each [`Extractor`] pairs an input
//! matcher with an extraction function, and the [`ExtractorRegistry`] asks
//! them in priority order — the first whose `matches` returns true handles
//! the input. The plain-text fallback is registered last so URLs are never
//! swallowed by it.

pub mod plain;
pub mod spotify;

use std::fmt;

use rust_messenger::traits;
use rust_messenger::traits::extended::Sender;
use serde::{Deserialize, Serialize};

use crate::config::{AppContext, Config};
use crate::messages::{ConfigChanged, ExtractRequest, ExtractResult, HandlerId};

/// One Soulseek search derived from the input.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SearchJob {
    pub artist: Option<String>,
    pub title: Option<String>,
    pub album: Option<String>,
    /// Verbatim query; takes precedence over the structured fields.
    pub raw_query: Option<String>,
}

impl SearchJob {
    pub fn to_query(&self) -> String {
        if let Some(raw) = &self.raw_query {
            return raw.clone();
        }
        let mut parts = Vec::new();
        if let Some(artist) = &self.artist {
            parts.push(artist.as_str());
        }
        if let Some(title) = &self.title {
            parts.push(title.as_str());
        }
        parts.join(" ")
    }
}

/// The extraction result: where the jobs came from plus the jobs themselves.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub source_label: String,
    pub searches: Vec<SearchJob>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExtractError {
    EmptyInput,
    MissingCredentials(String),
    Api(String),
    Parse(String),
}

impl fmt::Display for ExtractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExtractError::EmptyInput => write!(f, "input is empty"),
            ExtractError::MissingCredentials(what) => {
                write!(f, "missing credentials: {what}")
            }
            ExtractError::Api(err) => write!(f, "api error: {err}"),
            ExtractError::Parse(err) => write!(f, "parse error: {err}"),
        }
    }
}

/// A source of search jobs. The sockseek `IInputMatcher` + `IExtractor` pair.
pub trait Extractor: Send {
    /// Cheap syntactic test: does this input belong to this extractor?
    fn matches(&self, input: &str) -> bool;
    /// Turn the input into jobs. May block on network calls; the extractor
    /// component runs on its own worker so this cannot stall the core.
    fn extract(&self, input: &str, config: &Config) -> Result<Job, ExtractError>;
}

/// Priority-ordered registry: the first matching extractor wins.
pub struct ExtractorRegistry {
    extractors: Vec<Box<dyn Extractor>>,
}

impl ExtractorRegistry {
    pub fn new(extractors: Vec<Box<dyn Extractor>>) -> Self {
        ExtractorRegistry { extractors }
    }

    /// The standard set: Spotify URLs first, plain text search as fallback.
    pub fn standard(api: Box<dyn spotify::SpotifyApi>) -> Self {
        ExtractorRegistry::new(vec![
            Box::new(spotify::SpotifyExtractor::new(api)),
            Box::new(plain::PlainQueryExtractor),
        ])
    }

    pub fn extract(&self, input: &str, config: &Config) -> Result<Job, ExtractError> {
        let input = input.trim();
        if input.is_empty() {
            return Err(ExtractError::EmptyInput);
        }
        for extractor in &self.extractors {
            if extractor.matches(input) {
                return extractor.extract(input, config);
            }
        }
        Err(ExtractError::Parse("no extractor matches this input".into()))
    }
}

/// Bus component wrapping the registry. Lives on its own worker
/// (`ExtractWorker`) because `extract` may block on HTTP for seconds.
pub struct ExtractorComponent {
    registry: ExtractorRegistry,
    config: Config,
}

impl ExtractorComponent {
    pub fn new<W: traits::core::Writer>(ctx: &AppContext, _writer: &W) -> Self {
        ExtractorComponent {
            registry: ExtractorRegistry::standard(Box::new(spotify::UreqSpotifyApi::new())),
            config: ctx.config.clone(),
        }
    }

    #[cfg(test)]
    pub fn with_registry(registry: ExtractorRegistry, config: Config) -> Self {
        ExtractorComponent { registry, config }
    }
}

impl traits::core::Handler for ExtractorComponent {
    type Id = HandlerId;
    const ID: HandlerId = HandlerId::Extractor;
}

impl traits::core::Handle<ExtractRequest> for ExtractorComponent {
    fn handle<W: traits::core::Writer>(&mut self, message: &ExtractRequest, writer: &W) {
        let result = self
            .registry
            .extract(&message.input, &self.config)
            .map_err(|e| e.to_string());
        Self::send(&ExtractResult { corr: message.corr, result }, writer);
    }
}

impl traits::core::Handle<ConfigChanged> for ExtractorComponent {
    fn handle<W: traits::core::Writer>(&mut self, message: &ConfigChanged, _writer: &W) {
        self.config = message.config.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedExtractor {
        prefix: &'static str,
        label: &'static str,
    }

    impl Extractor for FixedExtractor {
        fn matches(&self, input: &str) -> bool {
            input.starts_with(self.prefix)
        }
        fn extract(&self, input: &str, _config: &Config) -> Result<Job, ExtractError> {
            Ok(Job {
                source_label: self.label.into(),
                searches: vec![SearchJob { raw_query: Some(input.into()), ..Default::default() }],
            })
        }
    }

    #[test]
    fn first_matching_extractor_wins() {
        let registry = ExtractorRegistry::new(vec![
            Box::new(FixedExtractor { prefix: "spotify", label: "first" }),
            Box::new(FixedExtractor { prefix: "", label: "fallback" }),
        ]);
        let config = Config::default();
        assert_eq!(registry.extract("spotify-thing", &config).unwrap().source_label, "first");
        assert_eq!(registry.extract("anything else", &config).unwrap().source_label, "fallback");
    }

    #[test]
    fn empty_input_is_rejected_before_matching() {
        let registry = ExtractorRegistry::new(vec![Box::new(FixedExtractor {
            prefix: "",
            label: "fallback",
        })]);
        assert_eq!(
            registry.extract("   ", &Config::default()),
            Err(ExtractError::EmptyInput)
        );
    }

    #[test]
    fn search_job_query_prefers_raw() {
        let job = SearchJob {
            artist: Some("Artist".into()),
            title: Some("Title".into()),
            raw_query: Some("verbatim".into()),
            ..Default::default()
        };
        assert_eq!(job.to_query(), "verbatim");

        let job = SearchJob {
            artist: Some("Artist".into()),
            title: Some("Title".into()),
            ..Default::default()
        };
        assert_eq!(job.to_query(), "Artist Title");
    }
}
