use serde::{Deserialize, Serialize};

pub const CURRENT_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub monitoring: MonitoringConfig,
    #[serde(default)]
    pub ocr: OcrConfig,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub privacy: PrivacyConfig,
    #[serde(default)]
    pub notifications: NotificationsConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub log: LogConfig,
}

fn default_schema_version() -> u32 {
    CURRENT_SCHEMA_VERSION
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            monitoring: MonitoringConfig::default(),
            ocr: OcrConfig::default(),
            storage: StorageConfig::default(),
            privacy: PrivacyConfig::default(),
            notifications: NotificationsConfig::default(),
            mcp: McpConfig::default(),
            ui: UiConfig::default(),
            log: LogConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MonitoringConfig {
    pub enabled: bool,
    pub poll_interval_ms: u64,
    pub min_length: usize,
    pub ignore_patterns: Vec<String>,
    pub ignore_own_log_paths: bool,
}

impl Default for MonitoringConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            // 500 ms matches Maccy's default and halves wakeups vs the
            // previous 250 ms without a perceptible latency change —
            // clipboard capture isn't latency-sensitive. The monitor
            // loop also applies exponential backoff when idle, so
            // sustained idle polling is well below this rate.
            poll_interval_ms: 500,
            min_length: 10,
            ignore_patterns: vec![
                r"^sk-[A-Za-z0-9]{20,}".into(),
                r"^\w+_KEY\s*=".into(),
                r"\b\d{4}[- ]?\d{4}[- ]?\d{4}[- ]?\d{4}\b".into(),
                r"(?i)password\s*[=:]\s*\S+".into(),
            ],
            ignore_own_log_paths: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OcrConfig {
    pub enabled: bool,
    pub recognition_level: String,
    pub languages: Vec<String>,
    pub min_confidence: f32,
    pub image_max_dimension: u32,
}

impl Default for OcrConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            recognition_level: "accurate".into(),
            languages: vec!["en-US".into()],
            min_confidence: 0.4,
            image_max_dimension: 4096,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StorageConfig {
    pub log_dir: String,
    pub sqlite_path: String,
    pub ring_buffer_size: usize,
    pub date_format: String,
    pub max_md_file_size_mb: usize,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            log_dir: "~/textlog/logs".into(),
            sqlite_path: "~/textlog/index.db".into(),
            ring_buffer_size: 1000,
            date_format: "%Y-%m-%d".into(),
            max_md_file_size_mb: 10,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PrivacyConfig {
    pub filter_enabled: bool,
    pub log_sensitive: bool,
    pub show_filter_notification: bool,
}

impl Default for PrivacyConfig {
    fn default() -> Self {
        Self {
            filter_enabled: true,
            log_sensitive: false,
            show_filter_notification: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NotificationsConfig {
    pub enabled: bool,
    pub on_capture: bool,
    pub on_complete: bool,
    pub copy_log_path_on_complete: bool,
    pub sound: bool,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            on_capture: false,
            on_complete: true,
            copy_log_path_on_complete: false,
            sound: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpConfig {
    pub max_recent: u32,
    pub max_search_limit: u32,
    pub max_search_results_bytes: usize,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            max_recent: 100,
            max_search_limit: 200,
            max_search_results_bytes: 65536,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UiConfig {
    pub pager: String,
    pub color_output: String,
    pub timestamp_format: String,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            pager: "less".into(),
            color_output: "auto".into(),
            timestamp_format: "%H:%M:%S".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LogConfig {
    pub level: String,
    pub format: String,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
            format: "pretty".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_schema_version_is_two() {
        assert_eq!(Config::default().schema_version, 2);
    }

    #[test]
    fn default_monitoring_matches_spec() {
        let m = MonitoringConfig::default();
        assert!(m.enabled);
        assert_eq!(m.poll_interval_ms, 500);
        assert_eq!(m.min_length, 10);
        assert!(m.ignore_own_log_paths);
        assert_eq!(m.ignore_patterns.len(), 4);
    }

    #[test]
    fn default_ocr_uses_apple_vision_defaults() {
        let o = OcrConfig::default();
        assert!(o.enabled);
        assert_eq!(o.recognition_level, "accurate");
        assert_eq!(o.languages, vec!["en-US"]);
        assert!((o.min_confidence - 0.4).abs() < f32::EPSILON);
        assert_eq!(o.image_max_dimension, 4096);
    }

    #[test]
    fn default_storage_uses_tilde_paths() {
        let s = StorageConfig::default();
        assert_eq!(s.log_dir, "~/textlog/logs");
        assert_eq!(s.sqlite_path, "~/textlog/index.db");
        assert_eq!(s.ring_buffer_size, 1000);
    }

    #[test]
    fn default_notifications_do_not_write_back_path() {
        let n = NotificationsConfig::default();
        assert!(n.enabled);
        assert!(!n.on_capture);
        assert!(n.on_complete);
        assert!(!n.copy_log_path_on_complete);
        assert!(!n.sound);
    }

    #[test]
    fn default_mcp_caps_are_bounded() {
        let m = McpConfig::default();
        assert_eq!(m.max_recent, 100);
        assert_eq!(m.max_search_limit, 200);
        assert_eq!(m.max_search_results_bytes, 65536);
    }

    #[test]
    fn config_roundtrips_through_toml() {
        let original = Config::default();
        let serialized = toml::to_string_pretty(&original).expect("serialize");
        let parsed: Config = toml::from_str(&serialized).expect("deserialize");
        assert_eq!(original, parsed);
    }

    #[test]
    fn partial_config_fills_in_defaults() {
        let partial = r#"
            schema_version = 2
            [monitoring]
            enabled = false
            poll_interval_ms = 500
            min_length = 20
            ignore_patterns = []
            ignore_own_log_paths = false
        "#;
        let cfg: Config = toml::from_str(partial).expect("parse");
        assert!(!cfg.monitoring.enabled);
        assert_eq!(cfg.monitoring.poll_interval_ms, 500);
        assert_eq!(cfg.ocr, OcrConfig::default());
        assert_eq!(cfg.storage, StorageConfig::default());
        assert_eq!(cfg.notifications, NotificationsConfig::default());
    }

    #[test]
    fn unknown_schema_version_still_parses() {
        let payload = r#"
            schema_version = 99
        "#;
        let cfg: Config = toml::from_str(payload).expect("parse");
        assert_eq!(cfg.schema_version, 99);
    }
}
