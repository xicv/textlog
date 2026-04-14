use std::collections::HashMap;

use super::schema::Config;

/// Apply environment variable overrides to a config.
///
/// Env takes precedence over the file (defaults → file → env → CLI flags).
pub fn apply_env(cfg: &mut Config) {
    let env: HashMap<String, String> = std::env::vars().collect();
    apply_env_map(cfg, &env);
}

/// Pure function used by tests: same logic as `apply_env` but over a caller-provided map.
pub fn apply_env_map(cfg: &mut Config, env: &HashMap<String, String>) {
    if let Some(v) = env.get("TEXTLOG_LOG_DIR") {
        cfg.storage.log_dir = v.clone();
    }
    if let Some(v) = env.get("TEXTLOG_SQLITE_PATH") {
        cfg.storage.sqlite_path = v.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn empty_env_is_noop() {
        let mut cfg = Config::default();
        let baseline = cfg.clone();
        apply_env_map(&mut cfg, &env(&[]));
        assert_eq!(cfg, baseline);
    }

    #[test]
    fn log_dir_override_applies() {
        let mut cfg = Config::default();
        apply_env_map(&mut cfg, &env(&[("TEXTLOG_LOG_DIR", "/tmp/other/logs")]));
        assert_eq!(cfg.storage.log_dir, "/tmp/other/logs");
    }

    #[test]
    fn sqlite_path_override_applies() {
        let mut cfg = Config::default();
        apply_env_map(&mut cfg, &env(&[("TEXTLOG_SQLITE_PATH", "/tmp/custom.db")]));
        assert_eq!(cfg.storage.sqlite_path, "/tmp/custom.db");
    }

    #[test]
    fn unrelated_env_vars_are_ignored() {
        let mut cfg = Config::default();
        let baseline = cfg.clone();
        apply_env_map(&mut cfg, &env(&[("HOME", "/home/x"), ("PATH", "/usr/bin")]));
        assert_eq!(cfg, baseline);
    }

    #[test]
    fn multiple_overrides_all_apply() {
        let mut cfg = Config::default();
        apply_env_map(
            &mut cfg,
            &env(&[("TEXTLOG_LOG_DIR", "/a"), ("TEXTLOG_SQLITE_PATH", "/b.db")]),
        );
        assert_eq!(cfg.storage.log_dir, "/a");
        assert_eq!(cfg.storage.sqlite_path, "/b.db");
    }
}
