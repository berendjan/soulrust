//! Configuration: the YAML file at `~/.config/soulrust.yaml`, the shared
//! [`AppContext`] every component is constructed from, and the
//! [`ConfigStore`] component that serves and persists config over the bus.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use rust_messenger::traits;
use rust_messenger::traits::extended::Sender;
use serde::{Deserialize, Serialize};

use crate::messages::{
    ConfigChanged, ConfigSnapshot, GetConfigReq, HandlerId, SetConfigReq, SetConfigResult,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    /// Port advertised to the server for incoming peer connections.
    pub listen_port: u32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            host: "server.slsknet.org".into(),
            port: 2242,
            username: String::new(),
            password: String::new(),
            listen_port: 2234,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SpotifyConfig {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    /// OAuth refresh token from the user-login flow; present once the user has
    /// logged in via `/spotify/login`. The extractor exchanges it for short-lived
    /// user access tokens.
    pub refresh_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateConfig {
    pub enabled: bool,
    pub auto_apply: bool,
    /// GitHub `owner/repo` the self-updater polls for releases.
    pub repo: String,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        UpdateConfig {
            enabled: true,
            auto_apply: true,
            repo: "berendjan/soulrust".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub bind_addr: String,
    /// Open the web UI in the OS default browser on startup. Default on; set
    /// false for headless/server installs.
    pub open_browser: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        UiConfig { bind_addr: "127.0.0.1:5030".into(), open_browser: true }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SharingConfig {
    /// Folders whose files we share with the network (public shares).
    pub folders: Vec<String>,
    pub download_dir: String,
    pub incomplete_dir: String,
    /// Concurrent upload slots.
    pub upload_slots: u32,
    /// FIFO queue ordering instead of per-user round-robin.
    pub fifo_queue: bool,
    /// Whether to answer incoming searches with our shared files.
    pub respond_to_searches: bool,
    /// Hard cap on files returned for a single incoming search. `u32` (not
    /// `usize`) so a config written on one platform parses identically on
    /// another.
    pub max_search_results: u32,
    /// Requester-side filtering of inbound search results: drop a peer's
    /// response with fewer than this many files (1 = keep any non-empty).
    pub min_result_files: u32,
    /// Drop a peer's response whose advertised upload speed is below this
    /// (bytes/s; 0 = no minimum).
    pub min_peer_upload_speed: u32,
    /// Drop a peer's response whose queue is longer than this (0 = no limit).
    pub max_peer_queue_length: u32,
    /// Aggregate download throttle in bytes/second across all transfers
    /// (0 = unlimited).
    pub max_download_speed: u32,
    /// Aggregate upload throttle in bytes/second across all transfers
    /// (0 = unlimited).
    pub max_upload_speed: u32,
}

impl SharingConfig {
    /// Where finished downloads land. Falls back to a per-OS default under the
    /// user's home (`~/Downloads/soulrust`) when unset, so downloads always have
    /// a real home even on a fresh config.
    pub fn download_path(&self) -> PathBuf {
        if self.download_dir.trim().is_empty() {
            home_dir().join("Downloads").join("soulrust")
        } else {
            PathBuf::from(&self.download_dir)
        }
    }

    /// Where in-progress (`INCOMPLETE-…`) files live. Defaults to an `incomplete`
    /// subfolder of the download folder when unset.
    pub fn incomplete_path(&self) -> PathBuf {
        if self.incomplete_dir.trim().is_empty() {
            self.download_path().join("incomplete")
        } else {
            PathBuf::from(&self.incomplete_dir)
        }
    }
}

/// The user's home directory, cross-platform: `HOME` on Linux/macOS,
/// `USERPROFILE` on Windows. Falls back to the current directory if neither is
/// set (headless/sandboxed environments).
fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

impl Default for SharingConfig {
    fn default() -> Self {
        SharingConfig {
            folders: Vec::new(),
            download_dir: String::new(),
            incomplete_dir: String::new(),
            upload_slots: 2,
            fifo_queue: false,
            respond_to_searches: true,
            max_search_results: 100,
            min_result_files: 1,
            min_peer_upload_speed: 0,
            max_peer_queue_length: 0,
            max_download_speed: 0,
            max_upload_speed: 0,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub spotify: SpotifyConfig,
    pub update: UpdateConfig,
    pub ui: UiConfig,
    pub sharing: SharingConfig,
}

// Conversions between the serde `Config` (used for YAML + field reads) and the
// buffa `Config` carried by ConfigSnapshot / SetConfigReq / ConfigChanged.
use soulrust_proto::bus;
use soulrust_proto::MessageField;

pub fn config_to_proto(c: &Config) -> bus::Config {
    bus::Config {
        server: MessageField::some(bus::ServerConfig {
            host: c.server.host.clone(),
            port: u32::from(c.server.port),
            username: c.server.username.clone(),
            password: c.server.password.clone(),
            listen_port: c.server.listen_port,
            ..Default::default()
        }),
        spotify: MessageField::some(bus::SpotifyConfig {
            client_id: c.spotify.client_id.clone(),
            client_secret: c.spotify.client_secret.clone(),
            refresh_token: c.spotify.refresh_token.clone(),
            ..Default::default()
        }),
        update: MessageField::some(bus::UpdateConfig {
            enabled: c.update.enabled,
            auto_apply: c.update.auto_apply,
            repo: c.update.repo.clone(),
            ..Default::default()
        }),
        ui: MessageField::some(bus::UiConfig {
            bind_addr: c.ui.bind_addr.clone(),
            open_browser: Some(c.ui.open_browser),
            ..Default::default()
        }),
        sharing: MessageField::some(bus::SharingConfig {
            folders: c.sharing.folders.clone(),
            download_dir: c.sharing.download_dir.clone(),
            incomplete_dir: c.sharing.incomplete_dir.clone(),
            upload_slots: c.sharing.upload_slots,
            fifo_queue: c.sharing.fifo_queue,
            respond_to_searches: c.sharing.respond_to_searches,
            max_search_results: c.sharing.max_search_results,
            min_result_files: c.sharing.min_result_files,
            min_peer_upload_speed: c.sharing.min_peer_upload_speed,
            max_peer_queue_length: c.sharing.max_peer_queue_length,
            max_download_speed: c.sharing.max_download_speed,
            max_upload_speed: c.sharing.max_upload_speed,
            ..Default::default()
        }),
        ..Default::default()
    }
}

pub fn config_from_proto(c: &bus::Config) -> Config {
    Config {
        server: ServerConfig {
            host: c.server.host.clone(),
            port: c.server.port as u16,
            username: c.server.username.clone(),
            password: c.server.password.clone(),
            listen_port: c.server.listen_port,
        },
        spotify: SpotifyConfig {
            client_id: c.spotify.client_id.clone(),
            client_secret: c.spotify.client_secret.clone(),
            refresh_token: c.spotify.refresh_token.clone(),
        },
        update: UpdateConfig {
            enabled: c.update.enabled,
            auto_apply: c.update.auto_apply,
            repo: c.update.repo.clone(),
        },
        ui: UiConfig {
            bind_addr: c.ui.bind_addr.clone(),
            // Absent (older config / default proto) means enabled.
            open_browser: c.ui.open_browser.unwrap_or(true),
        },
        sharing: SharingConfig {
            folders: c.sharing.folders.clone(),
            download_dir: c.sharing.download_dir.clone(),
            incomplete_dir: c.sharing.incomplete_dir.clone(),
            upload_slots: c.sharing.upload_slots,
            fifo_queue: c.sharing.fifo_queue,
            respond_to_searches: c.sharing.respond_to_searches,
            max_search_results: c.sharing.max_search_results,
            min_result_files: c.sharing.min_result_files,
            min_peer_upload_speed: c.sharing.min_peer_upload_speed,
            max_peer_queue_length: c.sharing.max_peer_queue_length,
            max_download_speed: c.sharing.max_download_speed,
            max_upload_speed: c.sharing.max_upload_speed,
        },
    }
}

/// `$XDG_CONFIG_HOME/soulrust.yaml`, falling back to `~/.config/soulrust.yaml`.
/// The home directory is resolved cross-platform (`HOME`, then `USERPROFILE`
/// on Windows) so the path is always absolute on a real desktop — a relative
/// `./.config/...` would otherwise resolve against the (GUI-dependent) process
/// CWD and be unreadable/unwritable, silently losing the saved config.
pub fn default_config_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("soulrust.yaml");
        }
    }
    home_dir().join(".config").join("soulrust.yaml")
}

/// Loads the config, falling back to defaults when the file is missing or
/// unparseable — startup must never fail on a bad config file.
pub fn load_config(path: &Path) -> Config {
    match std::fs::read_to_string(path) {
        Err(_) => Config::default(),
        Ok(text) => match serde_yaml::from_str(&text) {
            Ok(config) => config,
            Err(err) => {
                eprintln!(
                    "warning: failed to parse {} ({err}); using default configuration",
                    path.display()
                );
                Config::default()
            }
        },
    }
}

/// Writes atomically: serialize to `<path>.tmp`, then rename over the target.
/// Before overwriting, the previous config is copied to `<path>.old` (best
/// effort) so a bad write can be recovered, as Nicotine+ keeps a config backup.
pub fn save_config(path: &Path, config: &Config) -> Result<(), String> {
    let yaml = serde_yaml::to_string(config).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    // Keep a backup of the existing config before replacing it.
    if path.exists() {
        let backup = path.with_extension("yaml.old");
        let _ = std::fs::copy(path, backup);
    }
    let tmp = path.with_extension("yaml.tmp");
    std::fs::write(&tmp, yaml).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// Cross-thread control flags the web bridge sets and `main` polls.
#[derive(Debug, Default)]
pub struct Control {
    pub quit: AtomicBool,
    pub restart: AtomicBool,
}

/// The `Messenger!` config parameter: cloned into every worker thread and
/// passed to each component's constructor.
#[derive(Clone)]
pub struct AppContext {
    pub config: Config,
    pub config_path: PathBuf,
    pub control: Arc<Control>,
    /// Message bus ring size; a power of two. Generous so that a reader
    /// stalled in a slow handler can never be lapped in practice. The bus caps a
    /// single message at ~half this (the wrap size), so a larger ring also raises
    /// the per-message ceiling — 16 MiB here gives ~8 MiB per message, enough for
    /// a busy peer's search response to arrive whole in the common case.
    pub bus_buffer_size: usize,
}

impl AppContext {
    pub fn new(config: Config, config_path: PathBuf) -> Self {
        AppContext {
            config,
            config_path,
            control: Arc::new(Control::default()),
            bus_buffer_size: 16 * 1024 * 1024,
        }
    }
}

impl rust_messenger::message_bus::atomic_circular_bus::Config for AppContext {
    fn get_buffer_size(&self) -> usize {
        self.bus_buffer_size
    }
}

/// Bus component: owns the persisted configuration.
pub struct ConfigStore {
    path: PathBuf,
    current: Config,
}

impl ConfigStore {
    pub fn new<W: traits::core::Writer>(ctx: &AppContext, _writer: &W) -> Self {
        ConfigStore {
            path: ctx.config_path.clone(),
            current: ctx.config.clone(),
        }
    }

    fn validate(config: &Config) -> Result<(), String> {
        if config.server.host.trim().is_empty() {
            return Err("server host must not be empty".into());
        }
        if config.ui.bind_addr.parse::<std::net::SocketAddr>().is_err() {
            return Err(format!("invalid ui bind address: {}", config.ui.bind_addr));
        }
        Ok(())
    }
}

impl traits::core::Handler for ConfigStore {
    type Id = HandlerId;
    const ID: HandlerId = HandlerId::ConfigStore;
}

impl traits::core::Handle<GetConfigReq> for ConfigStore {
    fn handle<W: traits::core::Writer>(&mut self, message: &GetConfigReq, writer: &W) {
        Self::send(
            &ConfigSnapshot { corr: message.corr, config: MessageField::some(config_to_proto(&self.current)), ..Default::default() },
            writer,
        );
    }
}

impl traits::core::Handle<SetConfigReq> for ConfigStore {
    fn handle<W: traits::core::Writer>(&mut self, message: &SetConfigReq, writer: &W) {
        let config = config_from_proto(&message.config);
        let result = Self::validate(&config).and_then(|()| save_config(&self.path, &config));
        if result.is_ok() {
            self.current = config;
            Self::send(&ConfigChanged { config: MessageField::some(config_to_proto(&self.current)), ..Default::default() }, writer);
        }
        Self::send(&SetConfigResult { corr: message.corr, error: result.err(), ..Default::default() }, writer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("soulrust-test-{name}-{}", std::process::id()))
    }

    #[test]
    fn sharing_paths_fall_back_to_sane_defaults() {
        // Unset → per-OS default under the home directory.
        let cfg = SharingConfig::default();
        let dl = cfg.download_path();
        assert!(dl.ends_with("Downloads/soulrust"), "default download dir: {dl:?}");
        // Incomplete defaults to a subfolder of the download dir.
        assert_eq!(cfg.incomplete_path(), dl.join("incomplete"));

        // Explicit values are honored verbatim.
        let cfg = SharingConfig {
            download_dir: "/music/dl".into(),
            incomplete_dir: "/music/part".into(),
            ..SharingConfig::default()
        };
        assert_eq!(cfg.download_path(), PathBuf::from("/music/dl"));
        assert_eq!(cfg.incomplete_path(), PathBuf::from("/music/part"));
    }

    #[test]
    fn missing_file_yields_defaults() {
        let config = load_config(Path::new("/nonexistent/soulrust.yaml"));
        assert_eq!(config, Config::default());
        assert_eq!(config.server.host, "server.slsknet.org");
        assert_eq!(config.ui.bind_addr, "127.0.0.1:5030");
    }

    #[test]
    fn round_trips_through_yaml() {
        let dir = temp_path("roundtrip");
        let path = dir.join("soulrust.yaml");
        let mut config = Config::default();
        config.server.username = "alice".into();
        config.spotify.client_id = Some("id".into());

        save_config(&path, &config).unwrap();
        assert_eq!(load_config(&path), config);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_keeps_an_old_backup_of_the_previous_config() {
        let dir = temp_path("backup");
        let path = dir.join("soulrust.yaml");
        let old = path.with_extension("yaml.old");

        let mut first = Config::default();
        first.server.username = "first".into();
        save_config(&path, &first).unwrap();
        assert!(!old.exists(), "no backup is written on the first save");

        let mut second = Config::default();
        second.server.username = "second".into();
        save_config(&path, &second).unwrap();

        assert!(old.exists(), "the previous config is backed up to .old");
        assert_eq!(load_config(&path), second, "the live config is the latest write");
        assert_eq!(load_config(&old), first, "the .old backup holds the prior config");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn invalid_yaml_falls_back_to_defaults() {
        let path = temp_path("bad-yaml");
        std::fs::write(&path, ": not yaml [").unwrap();
        assert_eq!(load_config(&path), Config::default());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn partial_yaml_fills_in_defaults() {
        let path = temp_path("partial-yaml");
        std::fs::write(&path, "server:\n  username: bob\n").unwrap();
        let config = load_config(&path);
        assert_eq!(config.server.username, "bob");
        assert_eq!(config.server.host, "server.slsknet.org"); // defaulted
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn loads_populated_config_including_unicode() {
        // Parallels Nicotine+'s ConfigTest::test_load_config: a hand-written
        // config with explicit values (including a non-ASCII string) must load
        // back exactly, while unspecified sections still fall back to defaults.
        let path = temp_path("populated");
        std::fs::write(
            &path,
            "server:\n  \
             username: \"ääääääää\"\n  \
             password: pass123\n  \
             host: example.org\n  \
             port: 1234\nspotify:\n  \
             client_id: spotid\n",
        )
        .unwrap();

        let config = load_config(&path);
        // Non-ASCII strings survive the YAML round trip intact.
        assert_eq!(config.server.username, "ääääääää");
        assert_eq!(config.server.password, "pass123");
        assert_eq!(config.server.host, "example.org");
        assert_eq!(config.server.port, 1234);
        assert_eq!(config.spotify.client_id.as_deref(), Some("spotid"));
        // Unspecified field falls back to its default.
        assert_eq!(config.server.listen_port, ServerConfig::default().listen_port);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn validate_rejects_bad_bind_addr() {
        let mut config = Config::default();
        config.ui.bind_addr = "not-an-addr".into();
        assert!(ConfigStore::validate(&config).is_err());
    }
}
