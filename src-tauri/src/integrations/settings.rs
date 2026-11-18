use serde::{Deserialize, Serialize};

pub const CONTROL_API_SETTINGS_KEY: &str = "integrations.control_api";
pub const TUNA_SETTINGS_KEY: &str = "integrations.tuna";
pub const CONTROL_API_TOKEN_KEY: &str = "integrations.control_api_token";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControlApiSettings {
    pub enabled: bool,
    pub bind: String,
    pub port: u16,
}

impl Default for ControlApiSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: "127.0.0.1".to_string(),
            port: 27199,
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
        Self {
            enabled: false,
            url: "http://127.0.0.1:1608/".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControlApiStatus {
    pub enabled: bool,
    pub running: bool,
    pub bind_addr: Option<String>,
    pub port: Option<u16>,
    pub last_error: Option<String>,
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
        Self {
            control_api: ControlApiSettings::default(),
            tuna: TunaSettings::default(),
        }
    }
}

/// Validate and normalize control API settings. Returns normalized settings or Err.
pub fn validate_and_normalize_control_api_settings(mut s: ControlApiSettings) -> Result<ControlApiSettings, String> {
    let bind = s.bind.trim().to_string();
    let bind = if bind.eq_ignore_ascii_case("localhost") {
        "127.0.0.1".to_string()
    } else {
        bind
    };
    if bind != "127.0.0.1" && bind != "0.0.0.0" {
        if bind.parse::<std::net::IpAddr>().is_err() {
            return Err(format!("invalid bind address: {}", bind));
        }
    }
    if s.port == 0 || s.port < 1024 {
        return Err("port must be between 1024 and 65535".to_string());
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
        assert!(!s.tuna.enabled);
        assert_eq!(s.tuna.url, "http://127.0.0.1:1608/");
    }

    #[test]
    fn serde_roundtrip() {
        let s = IntegrationSettings::default();
        let j = serde_json::to_string(&s).unwrap();
        let s2: IntegrationSettings = serde_json::from_str(&j).unwrap();
        assert_eq!(s, s2);
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
