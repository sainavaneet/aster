//! Orbit decision tools for the MCP adapter and `aster orbit decisions|route`.
use serde_json::{json, Value};

/// `(tool, description, properties, required)` for the decision tools.
pub(super) fn tools() -> Vec<(&'static str, &'static str, Value, Vec<&'static str>)> {
    vec![
        ("orbit_route", "Suggest which team role should own a request, from Aster's decision layer ([polaris.decisions] routing). Read-only: nothing is assigned. Returns role, probability, and accepted (probability at least routing.accept_min); always confirm before calling orbit_agent_assign, because suggestions can be confidently wrong. Returns accepted false with a reason when routing decisions are disabled.", json!({
            "request":{"type":"string","minLength":1,"maxLength":4000,"description":"The request to route."}
        }), vec!["request"]),
        ("orbit_decisions", "List the last 50 decisions of Aster's decision layer (approvals, report checks, routing, interruption): kind, provider, redacted inputs, answers, thresholds, action, latency, and error. check true instead tests the provider key and endpoint without revealing the key.", json!({
            "check":{"type":"boolean","description":"Test the provider key with GET /v1/models."}
        }), vec![]),
    ]
}

/// Accepted arguments of a decision operation.
pub(super) fn allowed(operation: &str) -> Option<&'static [&'static str]> {
    match operation {
        "route" => Some(&["request"]),
        "decisions" => Some(&["check"]),
        _ => None,
    }
}

/// `aster orbit decisions --check` as tool arguments.
pub(super) fn check_arguments(args: &[String]) -> Option<Value> {
    match args {
        [operation, flag] if operation == "decisions" && flag == "--check" => {
            Some(json!({"check": true}))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_flag_maps_to_the_check_argument() {
        let args = |list: &[&str]| list.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            check_arguments(&args(&["decisions", "--check"])),
            Some(json!({"check": true}))
        );
        assert_eq!(check_arguments(&args(&["decisions"])), None);
        assert_eq!(allowed("route"), Some(&["request"][..]));
        assert_eq!(tools().len(), 2);
    }
}
