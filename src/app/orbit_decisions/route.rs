//! `orbit.route`: a suggested owner role for a request. Read-only: nothing is
//! assigned, and Polaris confirms before routing. Routing has been confidently
//! wrong in testing, so `accepted` only reports whether the probability passed
//! `accept_min`.
use std::collections::BTreeMap;

use serde_json::{json, Value};

use super::journal::{bounded, Entry, MAX_SUMMARY_BYTES};
use super::provider::{DecisionProvider, DecisionRequest, Question};
use crate::config::PolarisDecisionsConfig;

/// Longest request text accepted.
pub(crate) const MAX_REQUEST_BYTES: usize = 4_000;

#[derive(Debug, Clone)]
pub(crate) struct RouteInput {
    /// Redacted request text.
    pub(crate) request: String,
    /// `(role, area)` for each team role.
    pub(crate) roles: Vec<(String, String)>,
}

pub(crate) fn request(input: &RouteInput) -> DecisionRequest {
    let roster: Vec<Value> = input
        .roles
        .iter()
        .map(|(role, area)| json!({"role": role, "area": area}))
        .collect();
    let options: Vec<(&str, &str)> = input
        .roles
        .iter()
        .map(|(role, area)| (role.as_str(), area.as_str()))
        .collect();
    DecisionRequest {
        state: json!({"request": input.request, "roster": roster}),
        questions: BTreeMap::from([(
            "owner".to_owned(),
            Question::choice("Which role should own this request?", &options),
        )]),
    }
}

/// The result object and its journal entry.
pub(crate) fn decide(
    input: RouteInput,
    config: &PolarisDecisionsConfig,
    provider: &dyn DecisionProvider,
    now_ms: u64,
) -> (Value, Entry) {
    let started = std::time::Instant::now();
    let accept_min = config.routing.accept_min;
    let mut entry = Entry {
        ts: now_ms,
        kind: "routing".into(),
        provider: provider.name().into(),
        subject: None,
        inputs_summary: bounded(&format!("request: {}", input.request), MAX_SUMMARY_BYTES),
        answers: None,
        thresholds: json!({"accept_min": accept_min}),
        action: "none".into(),
        latency_ms: 0,
        error: None,
    };
    let result = match provider.evaluate(request(&input)) {
        Ok(answers) => {
            entry.answers = serde_json::to_value(&answers).ok();
            match answers.top_choice("owner") {
                Some((role, probability)) => {
                    let accepted = probability >= accept_min;
                    entry.action = format!("suggest {role}");
                    json!({
                        "role": role,
                        "probability": probability,
                        "accepted": accepted,
                        "provider": provider.name(),
                        "accept_min": accept_min,
                    })
                }
                None => {
                    json!({"role": null, "probability": null, "accepted": false, "provider": provider.name()})
                }
            }
        }
        Err(error) => {
            entry.error = Some(error.to_string());
            json!({
                "role": null,
                "probability": null,
                "accepted": false,
                "provider": provider.name(),
                "error": error.to_string(),
            })
        }
    };
    entry.latency_ms = started.elapsed().as_millis() as u64;
    (result, entry)
}

/// The answer when routing is not consulted.
pub(crate) fn disabled(provider: &str) -> Value {
    json!({
        "role": null,
        "probability": null,
        "accepted": false,
        "provider": provider,
        "reason": "routing decisions are disabled; set [polaris.decisions] provider = \"jev\" and routing.enabled = true",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::orbit_decisions::provider::Scripted;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/decisions/{name}.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    fn input() -> RouteInput {
        RouteInput {
            request: "rename parse_cfg to parse_config".into(),
            roles: vec![
                ("reviewer".into(), "code review, tests".into()),
                ("actor".into(), "edits and implementation".into()),
                ("researcher".into(), "reading docs and upstream code".into()),
            ],
        }
    }

    #[test]
    fn accepted_only_at_or_above_accept_min() {
        let config = PolarisDecisionsConfig::default();
        let route = |name: &str| decide(input(), &config, &Scripted::fixture(&fixture(name)), 0).0;
        let edit = route("route_edit");
        assert_eq!(edit["role"], "actor");
        assert_eq!(edit["accepted"], true);
        let check = route("route_check");
        assert_eq!(check["probability"], 0.8);
        assert_eq!(
            check["accepted"], true,
            "0.80 passes 0.75; Polaris still confirms"
        );
        let ambiguous = route("route_ambig");
        assert_eq!(ambiguous["role"], "researcher");
        assert_eq!(ambiguous["accepted"], false);
        let body = serde_json::to_value(request(&input())).unwrap();
        assert_eq!(
            body["questions"]["owner"]["criteria"]["actor"],
            "edits and implementation"
        );
    }
}
