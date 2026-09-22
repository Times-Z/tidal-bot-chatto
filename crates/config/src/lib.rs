use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::Path;
use std::time::Duration;
use thiserror::Error;
use url::Url;

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(3);
const DEFAULT_SAMPLE_RATE: u32 = 48_000;
const DEFAULT_VOLUME: u8 = 20;
const DEFAULT_TIDAL_TOKEN_PATH: &str = "tidal_token.json";

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("read config: {0}")]
    Read(std::io::Error),
    #[error("parse config: {0}")]
    Parse(serde_json::Error),
    #[error("invalid config: {0}")]
    Validation(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppConfig {
    pub chatto_url: String,
    pub chatto_token: String,
    pub tidal_token_path: String,
    pub tidal_quality: String,
    pub sample_rate: u32,
    pub livekit_url: String,
    pub rooms: Vec<String>,
    pub poll_interval: Duration,
    pub bot_name: String,
    pub volume: u8,
    pub default_lyrics: bool,
    /// Answer commands in the requesting message's thread.
    pub thread_replies: bool,
}

#[derive(Debug, Deserialize, Serialize)]
struct RawConfig {
    #[serde(default)]
    chatto_url: String,
    #[serde(default)]
    chatto_token: String,
    #[serde(default)]
    tidal_token_path: String,
    #[serde(default)]
    tidal_quality: String,
    #[serde(default)]
    sample_rate: Option<u32>,
    #[serde(default)]
    livekit_url: String,
    #[serde(default)]
    rooms: Vec<String>,
    #[serde(default)]
    poll_interval: Option<String>,
    #[serde(default)]
    bot_name: String,
    #[serde(default)]
    volume: Option<u16>,
    #[serde(default)]
    default_lyrics: bool,
    #[serde(default)]
    thread_replies: bool,
}

impl AppConfig {
    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let data = fs::read_to_string(path).map_err(ConfigError::Read)?;
        let mut raw: RawConfig = serde_json::from_str(&data).map_err(ConfigError::Parse)?;

        if raw.chatto_url.trim().is_empty() {
            raw.chatto_url = env::var("CHATTO_URL").unwrap_or_default();
        }
        if raw.chatto_token.trim().is_empty() {
            raw.chatto_token = env::var("CHATTO_TOKEN").unwrap_or_default();
        }
        if raw.tidal_token_path.trim().is_empty() {
            raw.tidal_token_path = DEFAULT_TIDAL_TOKEN_PATH.to_owned();
        }

        let poll_interval = raw
            .poll_interval
            .as_deref()
            .map(parse_duration)
            .transpose()?
            .unwrap_or(DEFAULT_POLL_INTERVAL);

        let volume_u16 = raw.volume.unwrap_or(u16::from(DEFAULT_VOLUME));
        let volume = u8::try_from(volume_u16)
            .map_err(|_| ConfigError::Validation("volume must be between 0 and 200".to_owned()))?;

        let config = AppConfig {
            chatto_url: raw.chatto_url,
            chatto_token: raw.chatto_token,
            tidal_token_path: raw.tidal_token_path,
            tidal_quality: raw.tidal_quality,
            sample_rate: raw.sample_rate.unwrap_or(DEFAULT_SAMPLE_RATE),
            livekit_url: raw.livekit_url,
            rooms: raw.rooms,
            poll_interval,
            bot_name: raw.bot_name,
            volume,
            default_lyrics: raw.default_lyrics,
            thread_replies: raw.thread_replies,
        };

        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.chatto_url.trim().is_empty() {
            return Err(ConfigError::Validation("chatto_url is required".to_owned()));
        }
        validate_url(&self.chatto_url, "chatto_url", &["http", "https"])?;

        if self.chatto_token.trim().is_empty() {
            return Err(ConfigError::Validation(
                "chatto_token is required".to_owned(),
            ));
        }

        if self.livekit_url.trim().is_empty() {
            return Err(ConfigError::Validation(
                "livekit_url is required".to_owned(),
            ));
        }
        validate_url(&self.livekit_url, "livekit_url", &["ws", "wss"])?;

        if self.rooms.is_empty() {
            return Err(ConfigError::Validation(
                "rooms must contain at least one room ID".to_owned(),
            ));
        }

        if let Some((idx, _)) = self
            .rooms
            .iter()
            .enumerate()
            .find(|(_, room)| room.trim().is_empty())
        {
            return Err(ConfigError::Validation(format!(
                "rooms[{idx}] must not be empty"
            )));
        }

        if self.poll_interval.is_zero() {
            return Err(ConfigError::Validation(
                "poll_interval must be > 0".to_owned(),
            ));
        }

        if self.volume > 200 {
            return Err(ConfigError::Validation(
                "volume must be between 0 and 200".to_owned(),
            ));
        }

        if self.sample_rate != 44_100 && self.sample_rate != 48_000 {
            return Err(ConfigError::Validation(
                "sample_rate must be 44100 or 48000".to_owned(),
            ));
        }

        if self.tidal_token_path.trim().is_empty() {
            return Err(ConfigError::Validation(
                "tidal_token_path must not be empty".to_owned(),
            ));
        }

        Ok(())
    }
}

fn parse_duration(input: &str) -> Result<Duration, ConfigError> {
    humantime::parse_duration(input)
        .map_err(|_| ConfigError::Validation("poll_interval must be a valid duration".to_owned()))
}

fn validate_url(raw: &str, field: &str, allowed_schemes: &[&str]) -> Result<(), ConfigError> {
    let parsed = Url::parse(raw)
        .map_err(|err| ConfigError::Validation(format!("{field} is not a valid URL: {err}")))?;

    if parsed.scheme().is_empty() || parsed.host_str().is_none() {
        return Err(ConfigError::Validation(format!(
            "{field} must include scheme and host"
        )));
    }

    if allowed_schemes
        .iter()
        .any(|scheme| parsed.scheme().eq_ignore_ascii_case(scheme))
    {
        return Ok(());
    }

    Err(ConfigError::Validation(format!(
        "{field} must use one of schemes: {}",
        allowed_schemes.join(", ")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn valid_config() -> AppConfig {
        AppConfig {
            chatto_url: "https://chat.example.com".to_owned(),
            chatto_token: "cht_token".to_owned(),
            tidal_token_path: "tidal_token.json".to_owned(),
            tidal_quality: String::new(),
            sample_rate: 48_000,
            livekit_url: "wss://livekit.example.com".to_owned(),
            rooms: vec!["room1".to_owned()],
            poll_interval: Duration::from_secs(3),
            bot_name: String::new(),
            volume: 20,
            default_lyrics: false,
            thread_replies: false,
        }
    }

    #[test]
    fn config_validate() {
        let mut cfg = valid_config();
        assert!(cfg.validate().is_ok());

        cfg.chatto_url.clear();
        assert!(cfg.validate().is_err());
        cfg = valid_config();

        cfg.chatto_url = "ftp://chat.example.com".to_owned();
        assert!(cfg.validate().is_err());
        cfg = valid_config();

        cfg.chatto_token.clear();
        assert!(cfg.validate().is_err());
        cfg = valid_config();

        cfg.livekit_url.clear();
        assert!(cfg.validate().is_err());
        cfg = valid_config();

        cfg.livekit_url = "https://livekit.example.com".to_owned();
        assert!(cfg.validate().is_err());
        cfg = valid_config();

        cfg.rooms.clear();
        assert!(cfg.validate().is_err());
        cfg = valid_config();

        cfg.rooms = vec!["room1".to_owned(), "   ".to_owned()];
        assert!(cfg.validate().is_err());
        cfg = valid_config();

        cfg.poll_interval = Duration::ZERO;
        assert!(cfg.validate().is_err());
        cfg = valid_config();

        cfg.volume = 201;
        assert!(cfg.validate().is_err());
        cfg = valid_config();

        cfg.sample_rate = 96_000;
        assert!(cfg.validate().is_err());
        cfg = valid_config();

        cfg.tidal_token_path.clear();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn parse_duration_valid() {
        assert_eq!(parse_duration("3s").unwrap(), Duration::from_secs(3));
        assert_eq!(parse_duration("1m").unwrap(), Duration::from_secs(60));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
    }

    #[test]
    fn parse_duration_invalid() {
        assert!(parse_duration("not-a-duration").is_err());
    }

    // Tests touching process environment variables must hold this lock so
    // they never race with each other (edition 2024 makes env mutation
    // unsafe precisely because of such races).
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn tmp_config_path(tag: &str) -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "chatto-config-{}-{tag}-{n}.json",
            std::process::id()
        ))
    }

    struct TempFile(std::path::PathBuf);

    impl TempFile {
        fn with_content(tag: &str, content: &str) -> Self {
            let path = tmp_config_path(tag);
            fs::write(&path, content).unwrap();
            Self(path)
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    const MINIMAL: &str = r#"{
        "chatto_url": "https://chat.example.com",
        "chatto_token": "cht_BK_x",
        "livekit_url": "wss://lk.example.com",
        "rooms": ["r1"]
    }"#;

    #[test]
    fn load_full_config_file() {
        let file = TempFile::with_content(
            "full",
            r#"{
                "chatto_url": "https://chat.example.com",
                "chatto_token": "cht_BK_x",
                "tidal_token_path": "custom_token.json",
                "tidal_quality": "HIGH",
                "sample_rate": 44100,
                "livekit_url": "wss://lk.example.com",
                "rooms": ["r1", "r2"],
                "poll_interval": "5s",
                "bot_name": "my_bot",
                "volume": 55,
                "default_lyrics": true,
                "thread_replies": true
            }"#,
        );
        let cfg = AppConfig::load_from_path(&file.0).unwrap();
        assert_eq!(cfg.chatto_url, "https://chat.example.com");
        assert_eq!(cfg.tidal_token_path, "custom_token.json");
        assert_eq!(cfg.tidal_quality, "HIGH");
        assert_eq!(cfg.sample_rate, 44_100);
        assert_eq!(cfg.rooms, vec!["r1".to_owned(), "r2".to_owned()]);
        assert_eq!(cfg.poll_interval, Duration::from_secs(5));
        assert_eq!(cfg.bot_name, "my_bot");
        assert_eq!(cfg.volume, 55);
        assert!(cfg.default_lyrics);
        assert!(cfg.thread_replies);
    }

    #[test]
    fn load_applies_defaults() {
        let file = TempFile::with_content("defaults", MINIMAL);
        let cfg = AppConfig::load_from_path(&file.0).unwrap();
        assert_eq!(cfg.poll_interval, DEFAULT_POLL_INTERVAL);
        assert_eq!(cfg.sample_rate, DEFAULT_SAMPLE_RATE);
        assert_eq!(cfg.volume, DEFAULT_VOLUME);
        assert_eq!(cfg.tidal_token_path, DEFAULT_TIDAL_TOKEN_PATH);
        assert_eq!(cfg.bot_name, "");
        assert!(!cfg.default_lyrics);
        assert!(!cfg.thread_replies);
    }

    #[test]
    fn empty_tidal_token_path_falls_back_to_default() {
        let file = TempFile::with_content(
            "tokenpath",
            r#"{
                "chatto_url": "https://chat.example.com",
                "chatto_token": "cht_BK_x",
                "tidal_token_path": "   ",
                "livekit_url": "wss://lk.example.com",
                "rooms": ["r1"]
            }"#,
        );
        let cfg = AppConfig::load_from_path(&file.0).unwrap();
        assert_eq!(cfg.tidal_token_path, "tidal_token.json");
    }

    #[test]
    fn load_missing_file_is_read_error() {
        let path = tmp_config_path("missing");
        let err = AppConfig::load_from_path(&path).unwrap_err();
        assert!(matches!(err, ConfigError::Read(_)), "got {err:?}");
    }

    #[test]
    fn load_invalid_json_is_parse_error() {
        let file = TempFile::with_content("bad-json", "{ not json");
        let err = AppConfig::load_from_path(&file.0).unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)), "got {err:?}");
    }

    #[test]
    fn load_rejects_bad_poll_interval() {
        let file = TempFile::with_content(
            "bad-poll",
            r#"{
                "chatto_url": "https://chat.example.com",
                "chatto_token": "cht_BK_x",
                "livekit_url": "wss://lk.example.com",
                "rooms": ["r1"],
                "poll_interval": "not-a-duration"
            }"#,
        );
        let err = AppConfig::load_from_path(&file.0).unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)), "got {err:?}");
        assert!(err.to_string().contains("poll_interval"));
    }

    #[test]
    fn load_rejects_out_of_range_volume() {
        // 300 fits in u16 but not u8: caught as a validation error.
        let file = TempFile::with_content(
            "vol-300",
            r#"{
                "chatto_url": "https://chat.example.com",
                "chatto_token": "cht_BK_x",
                "livekit_url": "wss://lk.example.com",
                "rooms": ["r1"],
                "volume": 300
            }"#,
        );
        let err = AppConfig::load_from_path(&file.0).unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)), "got {err:?}");

        // 250 fits in u8 but violates the 0..=200 rule in validate().
        let file = TempFile::with_content(
            "vol-250",
            r#"{
                "chatto_url": "https://chat.example.com",
                "chatto_token": "cht_BK_x",
                "livekit_url": "wss://lk.example.com",
                "rooms": ["r1"],
                "volume": 250
            }"#,
        );
        assert!(AppConfig::load_from_path(&file.0).is_err());

        // 65536 does not even fit in u16: serde parse error.
        let file = TempFile::with_content(
            "vol-65k",
            r#"{
                "chatto_url": "https://chat.example.com",
                "chatto_token": "cht_BK_x",
                "livekit_url": "wss://lk.example.com",
                "rooms": ["r1"],
                "volume": 65536
            }"#,
        );
        let err = AppConfig::load_from_path(&file.0).unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)), "got {err:?}");
    }

    #[test]
    fn load_rejects_bad_sample_rate() {
        let file = TempFile::with_content(
            "bad-rate",
            r#"{
                "chatto_url": "https://chat.example.com",
                "chatto_token": "cht_BK_x",
                "livekit_url": "wss://lk.example.com",
                "rooms": ["r1"],
                "sample_rate": 96000
            }"#,
        );
        let err = AppConfig::load_from_path(&file.0).unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)), "got {err:?}");
        assert!(err.to_string().contains("sample_rate"));
    }

    #[test]
    fn load_rejects_bad_urls_and_rooms() {
        // Wrong scheme for the Chatto URL.
        let file = TempFile::with_content(
            "ftp",
            r#"{
                "chatto_url": "ftp://chat.example.com",
                "chatto_token": "cht_BK_x",
                "livekit_url": "wss://lk.example.com",
                "rooms": ["r1"]
            }"#,
        );
        assert!(AppConfig::load_from_path(&file.0).is_err());

        // URL without host: rejected by the URL parser itself.
        let file = TempFile::with_content(
            "nohost",
            r#"{
                "chatto_url": "https://",
                "chatto_token": "cht_BK_x",
                "livekit_url": "wss://lk.example.com",
                "rooms": ["r1"]
            }"#,
        );
        let err = AppConfig::load_from_path(&file.0).unwrap_err();
        assert!(err.to_string().contains("empty host"), "got {err}");

        // Parses fine but has no host component: scheme-and-host error.
        let file = TempFile::with_content(
            "nohost2",
            r#"{
                "chatto_url": "mailto:someone@example.com",
                "chatto_token": "cht_BK_x",
                "livekit_url": "wss://lk.example.com",
                "rooms": ["r1"]
            }"#,
        );
        let err = AppConfig::load_from_path(&file.0).unwrap_err();
        assert!(err.to_string().contains("scheme and host"), "got {err}");

        // Empty rooms list.
        let file = TempFile::with_content(
            "norooms",
            r#"{
                "chatto_url": "https://chat.example.com",
                "chatto_token": "cht_BK_x",
                "livekit_url": "wss://lk.example.com",
                "rooms": []
            }"#,
        );
        assert!(AppConfig::load_from_path(&file.0).is_err());
    }

    #[test]
    fn env_fills_missing_url_and_token() {
        let _guard = env_lock();
        let old_url = env::var("CHATTO_URL").ok();
        let old_token = env::var("CHATTO_TOKEN").ok();

        unsafe {
            env::set_var("CHATTO_URL", "https://env.example.com");
            env::set_var("CHATTO_TOKEN", "cht_BK_env");
        }
        let result = (|| {
            let file = TempFile::with_content(
                "env-fallback",
                r#"{ "livekit_url": "wss://lk.example.com", "rooms": ["r1"] }"#,
            );
            AppConfig::load_from_path(&file.0)
        })();

        // Restore the ambient environment for the other tests.
        unsafe {
            match old_url {
                Some(v) => env::set_var("CHATTO_URL", v),
                None => env::remove_var("CHATTO_URL"),
            }
            match old_token {
                Some(v) => env::set_var("CHATTO_TOKEN", v),
                None => env::remove_var("CHATTO_TOKEN"),
            }
        }

        let cfg = result.unwrap();
        assert_eq!(cfg.chatto_url, "https://env.example.com");
        assert_eq!(cfg.chatto_token, "cht_BK_env");
    }

    #[test]
    fn explicit_file_values_win_over_env() {
        let _guard = env_lock();
        let old_url = env::var("CHATTO_URL").ok();
        unsafe {
            env::set_var("CHATTO_URL", "https://should-not-be-used.example.com");
        }
        let result = (|| {
            let file = TempFile::with_content("env-precedence", MINIMAL);
            AppConfig::load_from_path(&file.0)
        })();
        unsafe {
            match old_url {
                Some(v) => env::set_var("CHATTO_URL", v),
                None => env::remove_var("CHATTO_URL"),
            }
        }
        let cfg = result.unwrap();
        assert_eq!(cfg.chatto_url, "https://chat.example.com");
    }

    #[test]
    fn missing_chatto_url_without_env_is_validation_error() {
        let _guard = env_lock();
        let old = env::var("CHATTO_URL").ok();
        unsafe {
            env::remove_var("CHATTO_URL");
        }
        let result = (|| {
            let file = TempFile::with_content(
                "missing-url",
                r#"{ "chatto_token": "cht_BK_x", "livekit_url": "wss://lk.example.com", "rooms": ["r1"] }"#,
            );
            AppConfig::load_from_path(&file.0)
        })();
        unsafe {
            if let Some(v) = old {
                env::set_var("CHATTO_URL", v);
            }
        }
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("chatto_url is required"),
            "got {err}"
        );
    }
}
