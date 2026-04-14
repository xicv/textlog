pub mod overlay;
pub mod schema;

pub use schema::Config;

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Resolve the config file path, honouring TEXTLOG_CONFIG_DIR then `~/textlog/config.toml`.
pub fn default_config_path() -> Result<PathBuf> {
    if let Ok(d) = std::env::var("TEXTLOG_CONFIG_DIR") {
        return Ok(PathBuf::from(d).join("config.toml"));
    }
    let home = dirs::home_dir()
        .ok_or_else(|| Error::ConfigNotFound("home directory unavailable".into()))?;
    Ok(home.join("textlog").join("config.toml"))
}

pub fn load_from(path: &Path) -> Result<Config> {
    let text = fs::read_to_string(path)?;
    let cfg: Config = toml::from_str(&text)?;
    Ok(cfg)
}

pub fn save_to(path: &Path, cfg: &Config) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = toml::to_string_pretty(cfg)?;
    fs::write(path, text)?;
    Ok(())
}

/// Load from disk, or write defaults if the file does not exist. Used by `tl config reset` flow.
pub fn load_or_init(path: &Path) -> Result<Config> {
    if path.exists() {
        load_from(path)
    } else {
        let cfg = Config::default();
        save_to(path, &cfg)?;
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn save_then_load_roundtrips() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");
        let original = Config::default();
        save_to(&path, &original).unwrap();
        assert!(path.exists());
        let loaded = load_from(&path).unwrap();
        assert_eq!(original, loaded);
    }

    #[test]
    fn load_missing_returns_io_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("does_not_exist.toml");
        let err = load_from(&path).unwrap_err();
        assert!(matches!(err, Error::Io(_)));
    }

    #[test]
    fn load_or_init_writes_defaults_when_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("textlog").join("config.toml");
        let cfg = load_or_init(&path).unwrap();
        assert_eq!(cfg, Config::default());
        assert!(path.exists());
    }

    #[test]
    fn load_or_init_reads_existing_without_rewrite() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut customised = Config::default();
        customised.monitoring.poll_interval_ms = 999;
        save_to(&path, &customised).unwrap();
        let loaded = load_or_init(&path).unwrap();
        assert_eq!(loaded.monitoring.poll_interval_ms, 999);
    }

    #[test]
    fn garbage_toml_returns_parse_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        fs::write(&path, "= not valid = toml = =").unwrap();
        let err = load_from(&path).unwrap_err();
        assert!(matches!(err, Error::ConfigParse(_)));
    }
}
