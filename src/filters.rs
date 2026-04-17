//! Privacy filter: compiled regex patterns from `monitoring.ignore_patterns`.
//!
//! Patterns are compiled once at daemon start (see spec §Privacy filters), so
//! each clipboard event costs a single `RegexSet` sweep instead of re-parsing.

use regex::RegexSet;

use crate::config::schema::{MonitoringConfig, PrivacyConfig};
use crate::error::Result;

#[derive(Debug, Clone)]
pub struct PrivacyFilter {
    set: Option<RegexSet>,
    enabled: bool,
}

impl PrivacyFilter {
    pub fn compile(patterns: &[String], enabled: bool) -> Result<Self> {
        if !enabled || patterns.is_empty() {
            return Ok(Self { set: None, enabled });
        }
        let set = RegexSet::new(patterns)?;
        Ok(Self { set: Some(set), enabled })
    }

    pub fn from_config(monitoring: &MonitoringConfig, privacy: &PrivacyConfig) -> Result<Self> {
        Self::compile(&monitoring.ignore_patterns, privacy.filter_enabled)
    }

    pub fn disabled() -> Self {
        Self { set: None, enabled: false }
    }

    pub fn is_sensitive(&self, content: &str) -> bool {
        match (&self.set, self.enabled) {
            (Some(set), true) => set.is_match(content),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;

    fn default_filter() -> PrivacyFilter {
        PrivacyFilter::from_config(&MonitoringConfig::default(), &PrivacyConfig::default())
            .expect("default patterns must compile")
    }

    #[test]
    fn matches_openai_api_key() {
        let f = default_filter();
        assert!(f.is_sensitive("sk-1234567890abcdefghij"));
    }

    #[test]
    fn rejects_short_sk_prefix() {
        let f = default_filter();
        assert!(!f.is_sensitive("sk-tooShort"));
    }

    #[test]
    fn matches_env_var_assignment() {
        let f = default_filter();
        assert!(f.is_sensitive("API_KEY=sk-secret"));
    }

    #[test]
    fn matches_credit_card_with_dashes() {
        let f = default_filter();
        assert!(f.is_sensitive("Card: 4111-1111-1111-1111"));
    }

    #[test]
    fn matches_credit_card_unspaced() {
        let f = default_filter();
        assert!(f.is_sensitive("4111111111111111"));
    }

    #[test]
    fn matches_password_assignment_case_insensitive() {
        let f = default_filter();
        assert!(f.is_sensitive("Password: hunter2"));
        assert!(f.is_sensitive("password=hunter2"));
    }

    #[test]
    fn safe_content_passes_through() {
        let f = default_filter();
        assert!(!f.is_sensitive("This is safe text"));
        assert!(!f.is_sensitive("Meeting notes for Q2 planning"));
    }

    #[test]
    fn disabled_filter_matches_nothing() {
        let f = PrivacyFilter::disabled();
        assert!(!f.is_sensitive("sk-1234567890abcdefghij"));
        assert!(!f.is_sensitive("4111-1111-1111-1111"));
    }

    #[test]
    fn filter_enabled_false_short_circuits() {
        let monitoring = MonitoringConfig::default();
        let privacy = PrivacyConfig {
            filter_enabled: false,
            ..PrivacyConfig::default()
        };
        let f = PrivacyFilter::from_config(&monitoring, &privacy).unwrap();
        assert!(!f.is_sensitive("sk-1234567890abcdefghij"));
    }

    #[test]
    fn empty_patterns_match_nothing() {
        let f = PrivacyFilter::compile(&[], true).unwrap();
        assert!(!f.is_sensitive("sk-1234567890abcdefghij"));
    }

    #[test]
    fn invalid_regex_surfaces_at_compile() {
        let bad = vec!["(unclosed".to_string()];
        let err = PrivacyFilter::compile(&bad, true).unwrap_err();
        assert!(matches!(err, Error::FilterCompile(_)));
    }

    #[test]
    fn user_pattern_extends_defaults() {
        let mut monitoring = MonitoringConfig::default();
        monitoring.ignore_patterns.push(r"(?i)secret-token".into());
        let f = PrivacyFilter::from_config(&monitoring, &PrivacyConfig::default()).unwrap();
        assert!(f.is_sensitive("here is a Secret-Token in clipboard"));
        assert!(!f.is_sensitive("regular notes about the day"));
    }
}
