//! Orbit team tools for the MCP adapter and `aster orbit team-*`.
use serde_json::{json, Value};

/// `(tool, description, properties, required)` for the team tools.
pub(super) fn tools() -> Vec<(&'static str, &'static str, Value, Vec<&'static str>)> {
    let member = json!({"type":"object","properties":{
        "name":{"type":"string","description":"Agent name, unique across the team; also the pane title."},
        "kind":{"type":"string","enum":["claude","codex"]},
        "model":{"type":"string","description":"The agent CLI's own model name, within the worker allowlist."},
        "permission_mode":{"type":"string","enum":["manual","acceptEdits","auto","plan","dontAsk"],"description":"Claude members only."}
    },"required":["name","kind"],"additionalProperties":false});
    vec![
        ("orbit_team_define", "Create or replace a team role: a permanent tab in the Polaris workspace, named after the role and placed before the Orbit tab, whose members (1 to 4 Claude or Codex agents) live as panes in that tab and never exit. The first definition creates the tab without changing focus; the team supervisor then starts each member with the role's instructions, resumes its own conversation after a restart, and relaunches it if its agent exits. At most 8 roles. Redefining a role updates area, prompt, and members; members left out keep running unsupervised and are listed in released.", json!({
            "name":{"type":"string","description":"Role name, for example reviewer or actor."},
            "area":{"type":"string","description":"What the role owns, in free text."},
            "prompt":{"type":"string","maxLength":4096,"description":"Role instructions given to every member."},
            "members":{"type":"array","minItems":1,"maxItems":4,"items":member}
        }), vec!["name","area","members"]),
        ("orbit_team_remove", "Retire a team role: its members stop being supervised. Refused while a member has an assignment awaiting its report unless force cancels it. stop_members true also closes the role's tab and ends its members; otherwise the tab stays as ordinary panes.", json!({
            "name":{"type":"string"},
            "stop_members":{"type":"boolean","description":"Close the role tab, ending its members."},
            "force":{"type":"boolean","description":"Cancel assignments still awaiting a member's report."}
        }), vec!["name"]),
        ("orbit_team_list", "List the team: each role's area, prompt, tab, and members with kind, model, pane_id, terminal_id, state (running, starting, relaunching, gave_up, pending), agent_status, open_tasks, and resumable. Use it to know who owns what before routing work.", json!({}), vec![]),
    ]
}

/// Accepted arguments of a team operation.
pub(super) fn allowed(operation: &str) -> Option<&'static [&'static str]> {
    match operation {
        "team_define" => Some(&["name", "area", "prompt", "members"]),
        "team_remove" => Some(&["name", "stop_members", "force"]),
        "team_list" => Some(&[]),
        _ => None,
    }
}
