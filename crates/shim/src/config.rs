//! Runtime configuration, read from the environment at startup.
//!
//! Every value mirrors a module-level constant of the Python shim,
//! same name and same default.

use std::env;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const DEFAULT_SOCKET_PATH: &str = "/run/musegadget/musegadget.sock";
pub const DEFAULT_PORT: u16 = 8001;
pub const DEFAULT_TIMEOUT_S: u64 = 1800;
pub const DEFAULT_DISCOVERY_WAIT_S: u64 = 60;
pub const DEFAULT_DISCOVERY_POLL: Duration = Duration::from_secs(2);
pub const DEFAULT_REPLIES_DIR: &str = "/workspace/replies";
pub const DEFAULT_REPLY_WAIT_S: u64 = 600;
/// The upstream service never waits longer than this per turn.
pub const MAX_WAIT_S: u64 = 600;
pub const DEFAULT_REPLY_POLL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct Config {
    pub socket_path: String,
    pub port: u16,
    pub timeout: Duration,
    pub model_aliases: Vec<String>,
    pub discovery_wait: Duration,
    pub discovery_poll: Duration,
    pub replies_dir: PathBuf,
    pub reply_wait_s: u64,
    pub reply_poll: Duration,
    pub api_keys: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            socket_path: DEFAULT_SOCKET_PATH.to_string(),
            port: DEFAULT_PORT,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_S),
            model_aliases: Vec::new(),
            discovery_wait: Duration::from_secs(DEFAULT_DISCOVERY_WAIT_S),
            discovery_poll: DEFAULT_DISCOVERY_POLL,
            replies_dir: PathBuf::from(DEFAULT_REPLIES_DIR),
            reply_wait_s: DEFAULT_REPLY_WAIT_S,
            reply_poll: DEFAULT_REPLY_POLL,
            api_keys: Vec::new(),
        }
    }
}

fn env_u64(name: &str, default: u64) -> Result<u64, String> {
    match env::var(name) {
        Ok(raw) => raw
            .trim()
            .parse::<u64>()
            .map_err(|_| format!("{name}={raw:?} is not a number")),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(env::VarError::NotUnicode(_)) => Err(format!("{name} is not valid Unicode")),
    }
}

fn env_u16(name: &str, default: u16) -> Result<u16, String> {
    match env::var(name) {
        Ok(raw) => raw
            .trim()
            .parse::<u16>()
            .map_err(|_| format!("{name}={raw:?} is not a port")),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(env::VarError::NotUnicode(_)) => Err(format!("{name} is not valid Unicode")),
    }
}

fn env_list(name: &str) -> Vec<String> {
    env::var(name)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

impl Config {
    /// Read the configuration from the environment, like the Python
    /// shim's module-level `os.environ.get` calls.
    pub fn from_env() -> Result<Config, String> {
        Ok(Config {
            socket_path: env::var("MUSEGADGET_SOCKET")
                .unwrap_or_else(|_| DEFAULT_SOCKET_PATH.to_string()),
            port: env_u16("SHIM_PORT", DEFAULT_PORT)?,
            timeout: Duration::from_secs(env_u64("SHIM_TIMEOUT_S", DEFAULT_TIMEOUT_S)?),
            model_aliases: env_list("SHIM_MODEL_ALIASES"),
            discovery_wait: Duration::from_secs(env_u64(
                "SHIM_DISCOVERY_WAIT_S",
                DEFAULT_DISCOVERY_WAIT_S,
            )?),
            discovery_poll: DEFAULT_DISCOVERY_POLL,
            replies_dir: PathBuf::from(
                env::var("SHIM_REPLIES_DIR").unwrap_or_else(|_| DEFAULT_REPLIES_DIR.to_string()),
            ),
            reply_wait_s: env_u64("SHIM_REPLY_WAIT_S", DEFAULT_REPLY_WAIT_S)?,
            reply_poll: DEFAULT_REPLY_POLL,
            api_keys: env_list("SHIM_API_KEYS"),
        })
    }

    /// A config for tests: one alias, no keys, tiny waits.
    pub fn test_default() -> Config {
        Config {
            socket_path: "/nonexistent/gadget.sock".to_string(),
            port: 0,
            model_aliases: vec!["muse".to_string()],
            discovery_wait: Duration::from_secs(0),
            discovery_poll: Duration::from_millis(0),
            reply_wait_s: 30,
            reply_poll: Duration::from_millis(10),
            ..Config::default()
        }
    }
}

/// Log one line to stderr, `[shim HH:MM:SS] ...`, like the Python
/// shim. Only lengths, timings and event types are ever logged:
/// never message contents.
pub fn log(msg: &str) {
    eprintln!("[shim {}] {msg}", hhmmss());
}

fn hhmmss() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        % 86_400;
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_python() {
        let config = Config::default();
        assert_eq!(config.socket_path, "/run/musegadget/musegadget.sock");
        assert_eq!(config.port, 8001);
        assert_eq!(config.timeout, Duration::from_secs(1800));
        assert_eq!(config.discovery_wait, Duration::from_secs(60));
        assert_eq!(config.reply_wait_s, 600);
        assert_eq!(MAX_WAIT_S, 600);
    }
}
