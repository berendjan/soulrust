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
}

impl Default for UiConfig {
    fn default() -> Self {
        UiConfig { bind_addr: "127.0.0.1:5030".into() }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub spotify: SpotifyConfig,
    pub update: UpdateConfig,
    pub ui: UiConfig,
}

/// `$XDG_CONFIG_HOME/soulrust.yaml`, falling back to `~/.config/soulrust.yaml`.
pub fn default_config_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("soulrust.yaml");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config").join("soulrust.yaml")
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
pub fn save_config(path: &Path, config: &Config) -> Result<(), String> {
    let yaml = serde_yaml::to_string(config).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
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
    /// stalled in a slow handler can never be lapped in practice.
    pub bus_buffer_size: usize,
}

impl AppContext {
    pub fn new(config: Config, config_path: PathBuf) -> Self {
        AppContext {
            config,
            config_path,
            control: Arc::new(Control::default()),
            bus_buffer_size: 4 * 1024 * 1024,
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
            &ConfigSnapshot { corr: message.corr, config: self.current.clone() },
            writer,
        );
    }
}

impl traits::core::Handle<SetConfigReq> for ConfigStore {
    fn handle<W: traits::core::Writer>(&mut self, message: &SetConfigReq, writer: &W) {
        let result = Self::validate(&message.config)
            .and_then(|()| save_config(&self.path, &message.config));
        if result.is_ok() {
            self.current = message.config.clone();
            Self::send(&ConfigChanged { config: self.current.clone() }, writer);
        }
        Self::send(&SetConfigResult { corr: message.corr, result }, writer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("soulrust-test-{name}-{}", std::process::id()))
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
