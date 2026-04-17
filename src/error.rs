use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Configuration file not found: {0}")]
    ConfigNotFound(String),

    #[error("Failed to parse configuration: {0}")]
    ConfigParse(#[from] toml::de::Error),

    #[error("Failed to serialize configuration: {0}")]
    ConfigSerialize(#[from] toml::ser::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Clipboard access failed: {0}")]
    ClipboardAccess(String),

    #[error("Apple Vision OCR failed: {0}")]
    Ocr(String),

    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("Storage data error: {0}")]
    Storage(String),

    #[error("MCP protocol error: {0}")]
    Mcp(String),

    #[error("Notification dispatch failed: {0}")]
    Notification(String),

    #[error("launchctl operation failed: {0}")]
    Launchctl(String),

    #[error("Privacy filter compilation failed: {0}")]
    FilterCompile(#[from] regex::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_not_found_shows_path() {
        let err = Error::ConfigNotFound("config.toml".to_string());
        assert!(err.to_string().contains("config.toml"));
    }

    #[test]
    fn clipboard_access_shows_detail() {
        let err = Error::ClipboardAccess("permission denied".to_string());
        assert!(err.to_string().contains("permission denied"));
    }

    #[test]
    fn mcp_protocol_shows_detail() {
        let err = Error::Mcp("unknown tool foo".to_string());
        assert!(err.to_string().contains("unknown tool foo"));
    }

    #[test]
    fn ocr_error_shows_detail() {
        let err = Error::Ocr("vision request failed".to_string());
        assert!(err.to_string().contains("vision request failed"));
    }

    #[test]
    fn io_error_auto_converts() {
        fn fails() -> Result<()> {
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "gone"))?;
            Ok(())
        }
        let err = fails().unwrap_err();
        assert!(matches!(err, Error::Io(_)));
    }

    #[test]
    fn toml_parse_error_auto_converts() {
        let bad_toml = "not = valid = toml";
        let err: Error = toml::from_str::<toml::Value>(bad_toml).unwrap_err().into();
        assert!(matches!(err, Error::ConfigParse(_)));
    }

    #[test]
    fn regex_error_auto_converts() {
        let pattern = String::from("(") + "unclosed";
        let err: Error = regex::Regex::new(&pattern).unwrap_err().into();
        assert!(matches!(err, Error::FilterCompile(_)));
    }

    #[test]
    fn notification_shows_detail() {
        let err = Error::Notification("UNAuthorization denied".to_string());
        assert!(err.to_string().contains("UNAuthorization denied"));
    }

    #[test]
    fn launchctl_shows_detail() {
        let err = Error::Launchctl("bootstrap failed: EEXIST".to_string());
        assert!(err.to_string().contains("bootstrap failed"));
    }

    #[test]
    fn storage_shows_detail() {
        let err = Error::Storage("hex parse: bad length".to_string());
        assert!(err.to_string().contains("hex parse"));
    }

    #[test]
    fn sqlite_error_auto_converts() {
        let conn_err = rusqlite::Connection::open("/nonexistent/path/does/not/exist.db");
        let err: Error = conn_err.unwrap_err().into();
        assert!(matches!(err, Error::Sqlite(_)));
    }
}
