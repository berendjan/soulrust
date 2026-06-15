//! Spotify extractor: playlist/album/track URLs and URIs via the Web API
//! client-credentials flow. HTTP access is behind [`SpotifyApi`] so unit
//! tests run against JSON fixtures.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::extract::{ExtractError, Extractor, Job, SearchJob};

pub struct SpotifyToken {
    pub access_token: String,
    pub expires_at: Instant,
}

/// The two HTTP operations the extractor needs. Pagination and JSON mapping
/// stay above this boundary so they're covered by fixture tests.
pub trait SpotifyApi: Send {
    fn client_credentials_token(
        &self,
        client_id: &str,
        client_secret: &str,
    ) -> Result<SpotifyToken, String>;
    fn get_json(&self, access_token: &str, url: &str) -> Result<serde_json::Value, String>;
}

pub struct UreqSpotifyApi {
    agent: ureq::Agent,
}

impl UreqSpotifyApi {
    pub fn new() -> Self {
        UreqSpotifyApi {
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(15))
                .build(),
        }
    }
}

impl Default for UreqSpotifyApi {
    fn default() -> Self {
        Self::new()
    }
}

impl SpotifyApi for UreqSpotifyApi {
    fn client_credentials_token(
        &self,
        client_id: &str,
        client_secret: &str,
    ) -> Result<SpotifyToken, String> {
        // Credentials in the form body (supported alternative to the
        // Authorization: Basic header) to avoid a base64 dependency.
        let response: serde_json::Value = self
            .agent
            .post("https://accounts.spotify.com/api/token")
            .send_form(&[
                ("grant_type", "client_credentials"),
                ("client_id", client_id),
                ("client_secret", client_secret),
            ])
            .map_err(|e| format!("token request failed: {e}"))?
            .into_json()
            .map_err(|e| format!("token response is not json: {e}"))?;

        let access_token = response["access_token"]
            .as_str()
            .ok_or("token response missing access_token")?
            .to_owned();
        let expires_in = response["expires_in"].as_u64().unwrap_or(3600);
        Ok(SpotifyToken {
            access_token,
            // Refresh a minute early so requests never race expiry.
            expires_at: Instant::now() + Duration::from_secs(expires_in.saturating_sub(60)),
        })
    }

    fn get_json(&self, access_token: &str, url: &str) -> Result<serde_json::Value, String> {
        self.agent
            .get(url)
            .set("Authorization", &format!("Bearer {access_token}"))
            .call()
            .map_err(|e| format!("GET {url} failed: {e}"))?
            .into_json()
            .map_err(|e| format!("GET {url}: response is not json: {e}"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpotifyRef {
    Playlist(String),
    Album(String),
    Track(String),
}

/// Accepts `https://open.spotify.com/[intl-xx/]{playlist|album|track}/{id}[?...]`
/// and `spotify:{playlist|album|track}:{id}`.
pub fn parse_spotify_ref(input: &str) -> Option<SpotifyRef> {
    let input = input.trim();

    if let Some(rest) = input.strip_prefix("spotify:") {
        let mut parts = rest.split(':');
        let kind = parts.next()?;
        let id = parts.next()?;
        return make_ref(kind, id);
    }

    let rest = input
        .strip_prefix("https://")
        .or_else(|| input.strip_prefix("http://"))
        .unwrap_or(input);
    let rest = rest.strip_prefix("open.spotify.com/")?;
    let mut segments = rest.split('/');
    let mut kind = segments.next()?;
    if kind.starts_with("intl-") {
        kind = segments.next()?;
    }
    let id = segments.next()?;
    let id = id.split('?').next()?;
    make_ref(kind, id)
}

fn make_ref(kind: &str, id: &str) -> Option<SpotifyRef> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    match kind {
        "playlist" => Some(SpotifyRef::Playlist(id.into())),
        "album" => Some(SpotifyRef::Album(id.into())),
        "track" => Some(SpotifyRef::Track(id.into())),
        _ => None,
    }
}

pub struct SpotifyExtractor {
    api: Box<dyn SpotifyApi>,
    // Cached client-credentials token; RefCell because extract takes &self
    // and the component lives on a single worker thread.
    token: RefCell<Option<SpotifyToken>>,
}

impl SpotifyExtractor {
    pub fn new(api: Box<dyn SpotifyApi>) -> Self {
        SpotifyExtractor { api, token: RefCell::new(None) }
    }

    fn access_token(&self, config: &Config) -> Result<String, ExtractError> {
        let client_id = config
            .spotify
            .client_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ExtractError::MissingCredentials(
                    "spotify.client_id is not configured (see /config)".into(),
                )
            })?;
        let client_secret = config
            .spotify
            .client_secret
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ExtractError::MissingCredentials(
                    "spotify.client_secret is not configured (see /config)".into(),
                )
            })?;

        let mut cached = self.token.borrow_mut();
        if let Some(token) = cached.as_ref() {
            if token.expires_at > Instant::now() {
                return Ok(token.access_token.clone());
            }
        }
        let fresh = self
            .api
            .client_credentials_token(client_id, client_secret)
            .map_err(ExtractError::Api)?;
        let access = fresh.access_token.clone();
        *cached = Some(fresh);
        Ok(access)
    }

    fn track_job(track: &serde_json::Value, album: Option<&str>) -> Option<SearchJob> {
        let title = track["name"].as_str()?.to_owned();
        let artist = track["artists"][0]["name"].as_str().map(str::to_owned);
        let album = album
            .map(str::to_owned)
            .or_else(|| track["album"]["name"].as_str().map(str::to_owned));
        Some(SearchJob { artist, title: Some(title), album, raw_query: None })
    }

    fn extract_track(&self, token: &str, id: &str) -> Result<Job, ExtractError> {
        let track = self
            .api
            .get_json(token, &format!("https://api.spotify.com/v1/tracks/{id}"))
            .map_err(ExtractError::Api)?;
        let job = Self::track_job(&track, None)
            .ok_or_else(|| ExtractError::Parse("track response missing name".into()))?;
        Ok(Job {
            source_label: format!("spotify track: {}", job.title.as_deref().unwrap_or(id)),
            searches: vec![job],
        })
    }

    fn extract_album(&self, token: &str, id: &str) -> Result<Job, ExtractError> {
        let album = self
            .api
            .get_json(token, &format!("https://api.spotify.com/v1/albums/{id}"))
            .map_err(ExtractError::Api)?;
        let album_name = album["name"].as_str().unwrap_or(id).to_owned();

        let mut searches = Vec::new();
        let mut page = album["tracks"].clone();
        loop {
            for item in page["items"].as_array().into_iter().flatten() {
                if let Some(job) = Self::track_job(item, Some(&album_name)) {
                    searches.push(job);
                }
            }
            match page["next"].as_str() {
                Some(next) => {
                    page = self.api.get_json(token, next).map_err(ExtractError::Api)?;
                }
                None => break,
            }
        }

        Ok(Job { source_label: format!("spotify album: {album_name}"), searches })
    }

    fn extract_playlist(&self, token: &str, id: &str) -> Result<Job, ExtractError> {
        let meta = self
            .api
            .get_json(
                token,
                &format!("https://api.spotify.com/v1/playlists/{id}?fields=name"),
            )
            .map_err(ExtractError::Api)?;
        let playlist_name = meta["name"].as_str().unwrap_or(id).to_owned();

        let mut searches = Vec::new();
        let mut url = format!("https://api.spotify.com/v1/playlists/{id}/tracks?limit=100");
        loop {
            let page = self.api.get_json(token, &url).map_err(ExtractError::Api)?;
            for item in page["items"].as_array().into_iter().flatten() {
                // Playlist items wrap the track; local files have track: null.
                let track = &item["track"];
                if track.is_object() {
                    if let Some(job) = Self::track_job(track, None) {
                        searches.push(job);
                    }
                }
            }
            match page["next"].as_str() {
                Some(next) => url = next.to_owned(),
                None => break,
            }
        }

        Ok(Job { source_label: format!("spotify playlist: {playlist_name}"), searches })
    }
}

impl Extractor for SpotifyExtractor {
    fn matches(&self, input: &str) -> bool {
        parse_spotify_ref(input).is_some()
    }

    fn extract(&self, input: &str, config: &Config) -> Result<Job, ExtractError> {
        let spotify_ref = parse_spotify_ref(input)
            .ok_or_else(|| ExtractError::Parse("not a spotify url".into()))?;
        let token = self.access_token(config)?;
        match spotify_ref {
            SpotifyRef::Track(id) => self.extract_track(&token, &id),
            SpotifyRef::Album(id) => self.extract_album(&token, &id),
            SpotifyRef::Playlist(id) => self.extract_playlist(&token, &id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[test]
    fn parses_spotify_urls_and_uris() {
        assert_eq!(
            parse_spotify_ref("https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M?si=x"),
            Some(SpotifyRef::Playlist("37i9dQZF1DXcBWIGoYBM5M".into()))
        );
        assert_eq!(
            parse_spotify_ref("https://open.spotify.com/intl-nl/track/abc123"),
            Some(SpotifyRef::Track("abc123".into()))
        );
        assert_eq!(
            parse_spotify_ref("open.spotify.com/album/xyz"),
            Some(SpotifyRef::Album("xyz".into()))
        );
        assert_eq!(
            parse_spotify_ref("spotify:track:abc123"),
            Some(SpotifyRef::Track("abc123".into()))
        );
        assert_eq!(parse_spotify_ref("https://example.com/track/abc"), None);
        assert_eq!(parse_spotify_ref("plain search text"), None);
        assert_eq!(parse_spotify_ref("https://open.spotify.com/artist/abc"), None);
    }

    struct MockApi {
        responses: Mutex<HashMap<String, serde_json::Value>>,
        token_calls: std::sync::Arc<Mutex<u32>>,
        fail_token: bool,
    }

    impl MockApi {
        fn new(responses: Vec<(&str, serde_json::Value)>) -> Self {
            MockApi {
                responses: Mutex::new(
                    responses.into_iter().map(|(k, v)| (k.to_owned(), v)).collect(),
                ),
                token_calls: std::sync::Arc::new(Mutex::new(0)),
                fail_token: false,
            }
        }
    }

    impl SpotifyApi for MockApi {
        fn client_credentials_token(
            &self,
            _client_id: &str,
            _client_secret: &str,
        ) -> Result<SpotifyToken, String> {
            *self.token_calls.lock().unwrap() += 1;
            if self.fail_token {
                return Err("401 invalid_client".into());
            }
            Ok(SpotifyToken {
                access_token: "test-token".into(),
                expires_at: Instant::now() + Duration::from_secs(3600),
            })
        }

        fn get_json(&self, access_token: &str, url: &str) -> Result<serde_json::Value, String> {
            assert_eq!(access_token, "test-token");
            self.responses
                .lock()
                .unwrap()
                .get(url)
                .cloned()
                .ok_or_else(|| format!("unexpected url in test: {url}"))
        }
    }

    fn config_with_creds() -> Config {
        let mut config = Config::default();
        config.spotify.client_id = Some("id".into());
        config.spotify.client_secret = Some("secret".into());
        config
    }

    fn track(artist: &str, title: &str) -> serde_json::Value {
        json!({ "name": title, "artists": [{ "name": artist }] })
    }

    #[test]
    fn missing_credentials_is_a_clear_error() {
        let extractor = SpotifyExtractor::new(Box::new(MockApi::new(vec![])));
        let result = extractor.extract("spotify:track:abc", &Config::default());
        assert!(matches!(result, Err(ExtractError::MissingCredentials(_))));
    }

    #[test]
    fn token_failure_surfaces_as_api_error() {
        let mut api = MockApi::new(vec![]);
        api.fail_token = true;
        let extractor = SpotifyExtractor::new(Box::new(api));
        let result = extractor.extract("spotify:track:abc", &config_with_creds());
        assert!(matches!(result, Err(ExtractError::Api(e)) if e.contains("401")));
    }

    #[test]
    fn track_extraction() {
        let api = MockApi::new(vec![(
            "https://api.spotify.com/v1/tracks/abc",
            json!({
                "name": "Song",
                "artists": [{ "name": "Artist" }],
                "album": { "name": "Album" }
            }),
        )]);
        let extractor = SpotifyExtractor::new(Box::new(api));
        let job = extractor
            .extract("https://open.spotify.com/track/abc", &config_with_creds())
            .unwrap();
        assert_eq!(job.source_label, "spotify track: Song");
        assert_eq!(
            job.searches,
            vec![SearchJob {
                artist: Some("Artist".into()),
                title: Some("Song".into()),
                album: Some("Album".into()),
                raw_query: None,
            }]
        );
    }

    #[test]
    fn album_extraction_follows_pagination() {
        let api = MockApi::new(vec![
            (
                "https://api.spotify.com/v1/albums/alb",
                json!({
                    "name": "The Album",
                    "tracks": {
                        "items": [track("A", "One")],
                        "next": "https://api.spotify.com/v1/albums/alb/tracks?offset=1"
                    }
                }),
            ),
            (
                "https://api.spotify.com/v1/albums/alb/tracks?offset=1",
                json!({ "items": [track("A", "Two")], "next": null }),
            ),
        ]);
        let extractor = SpotifyExtractor::new(Box::new(api));
        let job = extractor.extract("spotify:album:alb", &config_with_creds()).unwrap();
        assert_eq!(job.source_label, "spotify album: The Album");
        assert_eq!(job.searches.len(), 2);
        assert_eq!(job.searches[1].title.as_deref(), Some("Two"));
        // Album name is propagated to every track.
        assert!(job.searches.iter().all(|s| s.album.as_deref() == Some("The Album")));
    }

    #[test]
    fn playlist_extraction_follows_pagination_and_skips_local_files() {
        let api = MockApi::new(vec![
            (
                "https://api.spotify.com/v1/playlists/pl?fields=name",
                json!({ "name": "Mix" }),
            ),
            (
                "https://api.spotify.com/v1/playlists/pl/tracks?limit=100",
                json!({
                    "items": [
                        { "track": track("X", "First") },
                        { "track": null }
                    ],
                    "next": "https://api.spotify.com/v1/playlists/pl/tracks?limit=100&offset=100"
                }),
            ),
            (
                "https://api.spotify.com/v1/playlists/pl/tracks?limit=100&offset=100",
                json!({ "items": [{ "track": track("Y", "Second") }], "next": null }),
            ),
        ]);
        let extractor = SpotifyExtractor::new(Box::new(api));
        let job = extractor
            .extract("https://open.spotify.com/playlist/pl", &config_with_creds())
            .unwrap();
        assert_eq!(job.source_label, "spotify playlist: Mix");
        assert_eq!(job.searches.len(), 2);
        assert_eq!(job.searches[0].to_query(), "X First");
        assert_eq!(job.searches[1].to_query(), "Y Second");
    }

    #[test]
    fn token_is_cached_across_extractions() {
        let api = MockApi::new(vec![(
            "https://api.spotify.com/v1/tracks/abc",
            track("A", "T"),
        )]);
        let token_calls = api.token_calls.clone();
        let extractor = SpotifyExtractor::new(Box::new(api));
        let config = config_with_creds();
        extractor.extract("spotify:track:abc", &config).unwrap();
        extractor.extract("spotify:track:abc", &config).unwrap();
        assert_eq!(*token_calls.lock().unwrap(), 1, "token must be fetched once and cached");
    }
}
