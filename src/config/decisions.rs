//! `[polaris.decisions]`: the typed decision layer in front of Orbit approvals,
//! report checks, routing suggestions, and delivery timing. Rules always run
//! first; a provider is consulted only when one is configured and the decision
//! is enabled. Key material is never part of this config, only where to find it.
use serde::{de, Deserialize, Deserializer, Serialize};

/// Bounds of `timeout_ms`, in milliseconds.
pub const MIN_DECISION_TIMEOUT_MS: u64 = 100;
pub const MAX_DECISION_TIMEOUT_MS: u64 = 30_000;

/// Where decisions beyond the rules come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DecisionProviderKind {
    /// Rules only; nothing leaves the machine.
    #[default]
    Rules,
    /// TypeSafe AI's Jev decision model over HTTPS.
    Jev,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct PolarisDecisionsConfig {
    /// rules (default, never calls out) or jev.
    pub provider: DecisionProviderKind,
    /// Jev model name.
    #[serde(deserialize_with = "model_name")]
    pub model: String,
    /// Environment variable holding the API key.
    #[serde(deserialize_with = "env_name")]
    pub api_key_env: String,
    /// File holding the API key, read when the variable is unset; `~` is expanded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_file: Option<String>,
    /// API base URL: https, or http only for a loopback host.
    #[serde(deserialize_with = "endpoint_url")]
    pub endpoint: String,
    /// Per-call limit in milliseconds, 100 to 30000.
    #[serde(deserialize_with = "timeout_ms")]
    pub timeout_ms: u64,
    pub approvals: DecisionApprovalsConfig,
    pub report_sanity: DecisionReportSanityConfig,
    pub routing: DecisionRoutingConfig,
    pub interruption: DecisionInterruptionConfig,
}

impl Default for PolarisDecisionsConfig {
    fn default() -> Self {
        Self {
            provider: DecisionProviderKind::Rules,
            model: "jev-latest".into(),
            api_key_env: "TYPESAFE_API_KEY".into(),
            api_key_file: None,
            endpoint: "https://api.typesafe.ai".into(),
            timeout_ms: 3_000,
            approvals: DecisionApprovalsConfig::default(),
            report_sanity: DecisionReportSanityConfig::default(),
            routing: DecisionRoutingConfig::default(),
            interruption: DecisionInterruptionConfig::default(),
        }
    }
}

/// Commands that always ask the user, matched anywhere in the command.
pub const DEFAULT_DENY_PATTERNS: [&str; 16] = [
    "rm -rf",
    "rm -fr",
    "rm -r -f",
    "git push --force",
    "git push -f",
    "git reset --hard",
    "sudo",
    "curl * | sh",
    "curl * | bash",
    "wget * | sh",
    "wget * | bash",
    "~/.ssh",
    ".ssh/",
    "id_rsa",
    ".env",
    "*.pem",
];

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct DecisionApprovalsConfig {
    /// Ask the provider about prompts no rule decides.
    pub enabled: bool,
    /// Minimum p(approve) to approve automatically.
    #[serde(deserialize_with = "probability")]
    pub approve_min: f64,
    /// Maximum p(ask_user) + p(deny) to approve automatically.
    #[serde(deserialize_with = "probability")]
    pub ask_or_deny_max: f64,
    /// Minimum risk mass in levels 0 and 1 to approve automatically.
    #[serde(deserialize_with = "probability")]
    pub low_risk_min: f64,
    /// Maximum risk mass in levels 3 and 4 to approve automatically.
    #[serde(deserialize_with = "probability")]
    pub high_risk_max: f64,
    /// Whole simple commands approved by the rules; `*` matches any text.
    pub allow_patterns: Vec<String>,
    /// Text that always sends the prompt to the user; `*` matches any text.
    pub deny_patterns: Vec<String>,
}

impl Default for DecisionApprovalsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            approve_min: 0.90,
            ask_or_deny_max: 0.10,
            low_risk_min: 0.85,
            high_risk_max: 0.02,
            allow_patterns: Vec::new(),
            deny_patterns: DEFAULT_DENY_PATTERNS.map(str::to_owned).to_vec(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct DecisionReportSanityConfig {
    /// Check worker reports against their assignment.
    pub enabled: bool,
    /// needs_followup at or above this flags the report.
    #[serde(deserialize_with = "probability")]
    pub followup_min: f64,
    /// answers_assignment score (0 to 4) below this flags the report.
    #[serde(deserialize_with = "score")]
    pub score_min: f64,
}

impl Default for DecisionReportSanityConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            followup_min: 0.80,
            score_min: 2.5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct DecisionRoutingConfig {
    /// Let orbit_route ask the provider for an owner role.
    pub enabled: bool,
    /// Minimum owner probability for accepted true.
    #[serde(deserialize_with = "probability")]
    pub accept_min: f64,
}

impl Default for DecisionRoutingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            accept_min: 0.75,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct DecisionInterruptionConfig {
    /// Ask before typing a report batch into an idle lead.
    pub enabled: bool,
    /// deliver_now below this defers delivery by 30 seconds.
    #[serde(deserialize_with = "probability")]
    pub deliver_min: f64,
}

impl Default for DecisionInterruptionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            deliver_min: 0.70,
        }
    }
}

fn probability<'de, D: Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
    bounded_number(deserializer, 0.0, 1.0)
}

fn score<'de, D: Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
    bounded_number(deserializer, 0.0, 4.0)
}

fn bounded_number<'de, D: Deserializer<'de>>(
    deserializer: D,
    min: f64,
    max: f64,
) -> Result<f64, D::Error> {
    let value = f64::deserialize(deserializer)?;
    if value.is_finite() && (min..=max).contains(&value) {
        Ok(value)
    } else {
        Err(de::Error::custom(format!(
            "decision threshold {value} must be between {min} and {max}"
        )))
    }
}

fn timeout_ms<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    let value = u64::deserialize(deserializer)?;
    if (MIN_DECISION_TIMEOUT_MS..=MAX_DECISION_TIMEOUT_MS).contains(&value) {
        Ok(value)
    } else {
        Err(de::Error::custom(format!(
            "decisions timeout_ms must be between {MIN_DECISION_TIMEOUT_MS} and {MAX_DECISION_TIMEOUT_MS}"
        )))
    }
}

fn model_name<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let value = String::deserialize(deserializer)?;
    let valid = !value.is_empty()
        && value.len() <= 64
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'));
    if valid {
        Ok(value)
    } else {
        Err(de::Error::custom(
            "decisions model must be 1-64 letters, digits, '-', '_' or '.'",
        ))
    }
}

fn env_name<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let value = String::deserialize(deserializer)?;
    let valid = value
        .chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_')
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_');
    if valid {
        Ok(value)
    } else {
        Err(de::Error::custom(
            "decisions api_key_env must be an environment variable name",
        ))
    }
}

/// `https://host[:port][/path]`, or `http://` to a loopback host for local testing.
fn valid_decision_endpoint(value: &str) -> bool {
    let rest = if let Some(rest) = value.strip_prefix("https://") {
        rest
    } else if let Some(rest) = value.strip_prefix("http://") {
        let host = rest.split(['/', ':']).next().unwrap_or("");
        if !matches!(host, "127.0.0.1" | "localhost") {
            return false;
        }
        rest
    } else {
        return false;
    };
    !rest.is_empty()
        && !rest.starts_with(['/', ':'])
        && rest
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | ':' | '/' | '_'))
}

fn endpoint_url<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let value = String::deserialize(deserializer)?;
    let value = value.trim_end_matches('/').to_owned();
    if valid_decision_endpoint(&value) {
        Ok(value)
    } else {
        Err(de::Error::custom(
            "decisions endpoint must be an https URL (http only for 127.0.0.1 or localhost)",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<PolarisDecisionsConfig, toml::de::Error> {
        toml::from_str(text)
    }

    #[test]
    fn defaults_are_rules_only_with_probe_thresholds() {
        let config = PolarisDecisionsConfig::default();
        assert_eq!(config.provider, DecisionProviderKind::Rules);
        assert_eq!(config.model, "jev-latest");
        assert_eq!(config.api_key_env, "TYPESAFE_API_KEY");
        assert_eq!(config.endpoint, "https://api.typesafe.ai");
        assert_eq!(config.timeout_ms, 3_000);
        assert!(!config.approvals.enabled);
        assert_eq!(config.approvals.approve_min, 0.90);
        assert_eq!(config.approvals.ask_or_deny_max, 0.10);
        assert_eq!(config.approvals.low_risk_min, 0.85);
        assert_eq!(config.approvals.high_risk_max, 0.02);
        assert!(config.approvals.allow_patterns.is_empty());
        assert!(config.approvals.deny_patterns.iter().any(|p| p == "rm -rf"));
        assert_eq!(config.report_sanity.followup_min, 0.80);
        assert_eq!(config.report_sanity.score_min, 2.5);
        assert_eq!(config.routing.accept_min, 0.75);
        assert_eq!(config.interruption.deliver_min, 0.70);
        assert_eq!(parse("").unwrap(), config);
    }

    #[test]
    fn explicit_values_and_nested_tables_are_loaded() {
        let config = parse(
            "provider = \"jev\"\nmodel = \"jev-preview\"\napi_key_file = \"~/k\"\n\
             endpoint = \"http://127.0.0.1:9/\"\ntimeout_ms = 500\n\
             approvals = { enabled = true, approve_min = 0.95, allow_patterns = [\"date\"] }\n\
             [report_sanity]\nenabled = true\nscore_min = 3\n",
        )
        .unwrap();
        assert_eq!(config.provider, DecisionProviderKind::Jev);
        assert_eq!(config.model, "jev-preview");
        assert_eq!(config.api_key_file.as_deref(), Some("~/k"));
        assert_eq!(config.endpoint, "http://127.0.0.1:9");
        assert_eq!(config.timeout_ms, 500);
        assert!(config.approvals.enabled);
        assert_eq!(config.approvals.approve_min, 0.95);
        // Unset nested keys keep their defaults.
        assert_eq!(config.approvals.ask_or_deny_max, 0.10);
        assert_eq!(config.approvals.allow_patterns, ["date"]);
        assert!(!config.approvals.deny_patterns.is_empty());
        assert!(config.report_sanity.enabled);
        assert_eq!(config.report_sanity.score_min, 3.0);
    }

    #[test]
    fn invalid_values_are_rejected() {
        for text in [
            "provider = \"openai\"",
            "timeout_ms = 10",
            "timeout_ms = 60000",
            "endpoint = \"http://api.typesafe.ai\"",
            "endpoint = \"ftp://x\"",
            "endpoint = \"https://\"",
            "model = \"\"",
            "model = \"a b\"",
            "api_key_env = \"1KEY\"",
            "approvals = { approve_min = 1.5 }",
            "routing = { accept_min = -0.1 }",
            "report_sanity = { score_min = 5 }",
        ] {
            assert!(parse(text).is_err(), "{text}");
        }
    }
}
