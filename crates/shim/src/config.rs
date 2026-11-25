//! Runtime configuration, read from the environment at startup.
//!
//! Every value mirrors a module-level constant of the Python shim,
//! same name and same default.

use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::model::is_uuid;

pub const DEFAULT_SOCKET_PATH: &str = "/run/musegadget/musegadget.sock";
pub const DEFAULT_PORT: u16 = 8001;
pub const DEFAULT_TIMEOUT_S: u64 = 1800;
pub const DEFAULT_DISCOVERY_WAIT_S: u64 = 60;
pub const DEFAULT_DISCOVERY_POLL: Duration = Duration::from_secs(2);
pub const DEFAULT_REPLIES_DIR: &str = "/workspace/replies";
pub const DEFAULT_REPLY_WAIT_S: u64 = 600;
/// The context window reported per model in `GET /v1/models`.
/// A configured value, not a measured one: the gadget never reports it.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 1_000_000;
/// The upstream service never waits longer than this per turn.
pub const MAX_WAIT_S: u64 = 600;
pub const DEFAULT_REPLY_POLL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct Config {
    pub socket_path: String,
    pub port: u16,
    pub timeout: Duration,
    pub model_aliases: Vec<String>,
    /// Named sessions: lowercase name to session UUID.
    pub session_aliases: HashMap<String, String>,
    /// Custom prompt per named session, keyed by lowercase name.
    pub session_prompts: HashMap<String, String>,
    pub context_window: u64,
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
            session_aliases: HashMap::new(),
            session_prompts: HashMap::new(),
            context_window: DEFAULT_CONTEXT_WINDOW,
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

/// Parse `SHIM_SESSION_ALIASES`: a JSON object mapping a session
/// name to a session UUID, for example
/// `{"alice": "12345678-1234-1234-1234-1234567890ab"}`. Names are
/// matched case-insensitively; `main` is reserved and rejected.
pub fn parse_session_aliases(raw: &str) -> Result<HashMap<String, String>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(HashMap::new());
    }
    let parsed: HashMap<String, String> = serde_json::from_str(trimmed)
        .map_err(|err| format!("SHIM_SESSION_ALIASES is not a JSON object: {err}"))?;
    let mut out = HashMap::new();
    for (name, uuid) in parsed {
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty() || name.contains('/') || name.contains(char::is_whitespace) {
            return Err(format!(
                "SHIM_SESSION_ALIASES has an invalid session name {name:?}"
            ));
        }
        if name == "main" {
            return Err("SHIM_SESSION_ALIASES must not define \"main\"".to_string());
        }
        let uuid = uuid.trim().to_ascii_lowercase();
        if !is_uuid(&uuid) {
            return Err(format!(
                "SHIM_SESSION_ALIASES[{name}] is not a UUID: {uuid:?}"
            ));
        }
        if out.insert(name.clone(), uuid).is_some() {
            return Err(format!("SHIM_SESSION_ALIASES defines {name:?} twice"));
        }
    }
    Ok(out)
}

/// Parse `SHIM_SESSION_PROMPTS`: a JSON object mapping a session
/// name to its custom prompt. Every name must have a session alias;
/// prompts are empty when unset.
pub fn parse_session_prompts(
    raw: &str,
    aliases: &HashMap<String, String>,
) -> Result<HashMap<String, String>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(HashMap::new());
    }
    let parsed: HashMap<String, String> = serde_json::from_str(trimmed)
        .map_err(|err| format!("SHIM_SESSION_PROMPTS is not a JSON object: {err}"))?;
    let mut out = HashMap::new();
    for (name, prompt) in parsed {
        let name = name.trim().to_ascii_lowercase();
        if !aliases.contains_key(&name) {
            return Err(format!("SHIM_SESSION_PROMPTS[{name}] has no session alias"));
        }
        let prompt = prompt.trim().to_string();
        if prompt.is_empty() {
            return Err(format!("SHIM_SESSION_PROMPTS[{name}] is empty"));
        }
        out.insert(name, prompt);
    }
    Ok(out)
}

impl Config {
    /// Read the configuration from the environment, like the Python
    /// shim's module-level `os.environ.get` calls.
    pub fn from_env() -> Result<Config, String> {
        let session_aliases =
            parse_session_aliases(&env::var("SHIM_SESSION_ALIASES").unwrap_or_default())?;
        let session_prompts = parse_session_prompts(
            &env::var("SHIM_SESSION_PROMPTS").unwrap_or_default(),
            &session_aliases,
        )?;
        Ok(Config {
            socket_path: env::var("MUSEGADGET_SOCKET")
                .unwrap_or_else(|_| DEFAULT_SOCKET_PATH.to_string()),
            port: env_u16("SHIM_PORT", DEFAULT_PORT)?,
            timeout: Duration::from_secs(env_u64("SHIM_TIMEOUT_S", DEFAULT_TIMEOUT_S)?),
            model_aliases: env_list("SHIM_MODEL_ALIASES"),
            session_aliases,
            session_prompts,
            context_window: env_u64("SHIM_CONTEXT_WINDOW", DEFAULT_CONTEXT_WINDOW)?,
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
        assert_eq!(config.context_window, 1_000_000);
        assert_eq!(MAX_WAIT_S, 600);
        assert!(config.session_aliases.is_empty());
        assert!(config.session_prompts.is_empty());
    }

    #[test]
    fn parse_session_aliases_cases() {
        assert!(parse_session_aliases("").unwrap().is_empty());
        assert!(parse_session_aliases("  ").unwrap().is_empty());
        let one =
            parse_session_aliases(r#"{"Alice": "12345678-1234-1234-1234-1234567890AB"}"#).unwrap();
        assert_eq!(
            one.get("alice"),
            Some(&"12345678-1234-1234-1234-1234567890ab".to_string())
        );
        assert!(parse_session_aliases("not json").is_err());
        assert!(parse_session_aliases(r#"["alice"]"#).is_err());
        assert!(
            parse_session_aliases(r#"{"alice": "not-a-uuid"}"#)
                .unwrap_err()
                .contains("is not a UUID")
        );
        assert!(
            parse_session_aliases(r#"{"main": "12345678-1234-1234-1234-1234567890ab"}"#)
                .unwrap_err()
                .contains("must not define")
        );
        assert!(
            parse_session_aliases(r#"{"a/b": "12345678-1234-1234-1234-1234567890ab"}"#).is_err()
        );
        assert!(
            parse_session_aliases(
                r#"{"Alice": "12345678-1234-1234-1234-1234567890ab", "alice": "12345678-1234-1234-1234-1234567890ab"}"#
            )
            .unwrap_err()
            .contains("twice")
        );
    }

    #[test]
    fn parse_session_prompts_cases() {
        let aliases =
            parse_session_aliases(r#"{"alice": "12345678-1234-1234-1234-1234567890ab"}"#).unwrap();
        assert!(parse_session_prompts("", &aliases).unwrap().is_empty());
        let one = parse_session_prompts(r#"{"Alice": "  You are talking to Alice.  "}"#, &aliases)
            .unwrap();
        assert_eq!(
            one.get("alice"),
            Some(&"You are talking to Alice.".to_string())
        );
        assert!(
            parse_session_prompts(r#"{"bob": "hi"}"#, &aliases)
                .unwrap_err()
                .contains("has no session alias")
        );
        assert!(
            parse_session_prompts(r#"{"alice": "  "}"#, &aliases)
                .unwrap_err()
                .contains("is empty")
        );
        assert!(parse_session_prompts("not json", &aliases).is_err());
    }
}
