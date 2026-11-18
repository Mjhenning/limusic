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

/// Validate control API settings. Returns Err(message) if invalid.
pub fn validate_control_api_settings(s: &ControlApiSettings) -> Result<(), String> {
    // bind must be 127.0.0.1 or 0.0.0.0 (or a valid IP) — be lenient but validate basics
    let bind = s.bind.trim();
    if bind != "127.0.0.1" && bind != "0.0.0.0" && bind != "localhost" {
        // allow other IPs? plan says "validate the bind setting (IP address or the two presets)"
        // try parse as IpAddr
        if bind.parse::<std::net::IpAddr>().is_err() {
            return Err(format!("invalid bind address: {}", bind));
        }
    }
    if s.port == 0 {
        return Err("port must be between 1 and 65535".to_string());
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
        assert!(validate_control_api_settings(&s).is_ok());
        s.bind = "0.0.0.0".to_string();
        assert!(validate_control_api_settings(&s).is_ok());
        s.bind = "192.168.1.5".to_string();
        assert!(validate_control_api_settings(&s).is_ok());
    }

    #[test]
    fn validate_bind_bad() {
        let mut s = ControlApiSettings::default();
        s.bind = "not-an-ip".to_string();
        assert!(validate_control_api_settings(&s).is_err());
        s.bind = "127.0.0.1".to_string();
        s.port = 0;
        assert!(validate_control_api_settings(&s).is_err());
    }
}
