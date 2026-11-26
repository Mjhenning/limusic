use serde::{Deserialize, Serialize};

pub const CONTROL_API_SETTINGS_KEY: &str = "integrations.control_api";
pub const TUNA_SETTINGS_KEY: &str = "integrations.tuna";
pub const CONTROL_API_TOKEN_KEY: &str = "integrations.control_api_token";

/// Default number of queue rows on either side of the current track protected from a repeat
/// request. Stored with the Control API settings but read per request, so UI edits take effect
/// without restarting the listener.
pub const DEFAULT_QUEUE_DEDUPE_WINDOW: usize = 10;
pub const MAX_QUEUE_DEDUPE_WINDOW: usize = 50;

fn default_queue_dedupe_window() -> usize {
    DEFAULT_QUEUE_DEDUPE_WINDOW
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControlApiSettings {
    pub enabled: bool,
    pub bind: String,
    pub port: u16,
    /// Host opt-in for shared Listen Together control. Old saved payloads predate it and read as
    /// false. `#[serde(default)]` keeps that upgrade silent.
    #[serde(default)]
    pub shared_control: bool,
    /// Rows within this distance of the current track are protected from repeat requests. Old
    /// saved payloads predate it and read as the default.
    #[serde(default = "default_queue_dedupe_window")]
    pub queue_dedupe_window: usize,
}

impl Default for ControlApiSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: "127.0.0.1".to_string(),
            port: 27199,
            shared_control: false,
            queue_dedupe_window: DEFAULT_QUEUE_DEDUPE_WINDOW,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TunaSettings {
    pub enabled: bool,
    pub url: String,
}

impl Default for TunaSettings {
    fn default() -> Self {
        Self { enabled: false, url: "http://127.0.0.1:1608/".to_string() }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControlApiStatus {
    pub enabled: bool,
    pub running: bool,
    pub bind_addr: Option<String>,
    pub port: Option<u16>,
    pub last_error: Option<String>,
    /// Live WebSocket clients. Defaults for old saved status payloads.
    #[serde(default)]
    pub ws_clients: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TunaStatus {
    pub enabled: bool,
    pub url: String,
    pub last_ok_at_ms: Option<i64>,
    pub last_error: Option<String>,
    pub backoff_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IntegrationStatus {
    pub control_api: ControlApiStatus,
    pub tuna: TunaStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IntegrationSettings {
    pub control_api: ControlApiSettings,
    pub tuna: TunaSettings,
}

impl Default for IntegrationSettings {
    fn default() -> Self {
        Self { control_api: ControlApiSettings::default(), tuna: TunaSettings::default() }
    }
}

/// Validate and normalize control API settings. Returns normalized settings or Err.
pub fn validate_and_normalize_control_api_settings(
    mut s: ControlApiSettings,
) -> Result<ControlApiSettings, String> {
    let bind = s.bind.trim().to_string();
    let bind = if bind.eq_ignore_ascii_case("localhost") { "127.0.0.1".to_string() } else { bind };
    if bind != "127.0.0.1" && bind != "0.0.0.0" {
        if bind.parse::<std::net::IpAddr>().is_err() {
            return Err(format!("invalid bind address: {}", bind));
        }
    }
    if s.port == 0 || s.port < 1024 {
        return Err("port must be between 1024 and 65535".to_string());
    }
    if s.queue_dedupe_window > MAX_QUEUE_DEDUPE_WINDOW {
        return Err(format!("queue dedupe window must be between 0 and {MAX_QUEUE_DEDUPE_WINDOW}"));
    }
    s.bind = bind;
    Ok(s)
}

pub fn validate_tuna_url(url: &str) -> Result<(), String> {
    let u = url.trim();
    let lower = u.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err("tuna url must start with http:// or https://".to_string());
    }
    // Basic check: there must be something after scheme
    let rest = if lower.starts_with("https://") { &u[8..] } else { &u[7..] };
    if rest.trim().is_empty() || !rest.contains(|c: char| c != '/' && c != ' ') {
        return Err("tuna url must include a host".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let s = IntegrationSettings::default();
        assert!(!s.control_api.enabled);
        assert_eq!(s.control_api.bind, "127.0.0.1");
        assert_eq!(s.control_api.port, 27199);
        assert!(!s.control_api.shared_control);
        assert_eq!(s.control_api.queue_dedupe_window, DEFAULT_QUEUE_DEDUPE_WINDOW);
        assert!(!s.tuna.enabled);
        assert_eq!(s.tuna.url, "http://127.0.0.1:1608/");
    }

    #[test]
    fn serde_roundtrip() {
        let s = IntegrationSettings::default();
        let j = serde_json::to_string(&s).unwrap();
        let s2: IntegrationSettings = serde_json::from_str(&j).unwrap();
        assert_eq!(s, s2);
        // A payload saved before the fork added shared control still loads as opt-out.
        let old = r#"{"control_api":{"enabled":true,"bind":"0.0.0.0","port":27199},"tuna":{"enabled":false,"url":"http://127.0.0.1:1608/"}}"#;
        let old: IntegrationSettings = serde_json::from_str(old).unwrap();
        assert!(!old.control_api.shared_control);
        assert_eq!(old.control_api.queue_dedupe_window, DEFAULT_QUEUE_DEDUPE_WINDOW);
        // The short-lived relay field is ignored, so downgrading needs no migration.
        let legacy = r#"{"control_api":{"enabled":false,"bind":"127.0.0.1","port":27199,"shared_control":true,"shared_control_server":"wss://old.example/ws"},"tuna":{"enabled":false,"url":"http://127.0.0.1:1608/"}}"#;
        let legacy: IntegrationSettings = serde_json::from_str(legacy).unwrap();
        assert!(legacy.control_api.shared_control);
    }

    #[test]
    fn validate_bind_ok() {
        let mut s = ControlApiSettings::default();
        let s2 = validate_and_normalize_control_api_settings(s.clone()).unwrap();
        assert_eq!(s2.bind, "127.0.0.1");
        s.bind = "0.0.0.0".to_string();
        assert!(validate_and_normalize_control_api_settings(s.clone()).is_ok());
        s.bind = "192.168.1.5".to_string();
        assert!(validate_and_normalize_control_api_settings(s.clone()).is_ok());
        s.bind = "localhost".to_string();
        let s3 = validate_and_normalize_control_api_settings(s.clone()).unwrap();
        assert_eq!(s3.bind, "127.0.0.1");
    }

    #[test]
    fn validate_bind_bad() {
        let mut s = ControlApiSettings::default();
        s.bind = "not-an-ip".to_string();
        assert!(validate_and_normalize_control_api_settings(s.clone()).is_err());
        s.bind = "127.0.0.1".to_string();
        s.port = 0;
        assert!(validate_and_normalize_control_api_settings(s.clone()).is_err());
        s.port = 80;
        assert!(validate_and_normalize_control_api_settings(s.clone()).is_err());
    }

    #[test]
    fn validate_queue_dedupe_window() {
        let mut s = ControlApiSettings::default();
        for window in [0, 5, MAX_QUEUE_DEDUPE_WINDOW] {
            s.queue_dedupe_window = window;
            assert_eq!(
                validate_and_normalize_control_api_settings(s.clone()).unwrap().queue_dedupe_window,
                window
            );
        }
        s.queue_dedupe_window = MAX_QUEUE_DEDUPE_WINDOW + 1;
        assert!(validate_and_normalize_control_api_settings(s).is_err());
    }

    #[test]
    fn validate_tuna_url_ok() {
        assert!(validate_tuna_url("http://127.0.0.1:1608/").is_ok());
        assert!(validate_tuna_url("https://example.com/path").is_ok());
    }

    #[test]
    fn validate_tuna_url_bad() {
        assert!(validate_tuna_url("file:///etc/passwd").is_err());
        assert!(validate_tuna_url("ftp://example.com").is_err());
        assert!(validate_tuna_url("").is_err());
    }
}
