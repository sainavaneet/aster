//! End-to-end Orbit team roles against a real `aster server` with the fake
//! Claude and Codex agents of `orbit_support`: a role owns a tab between the
//! Polaris tab and the Orbit tab, its members are resumed after a restart in
//! their own panes, and a member whose agent exits is relaunched there.
#![cfg(unix)]

mod orbit_support;
pub mod support;

use std::time::Duration;

use orbit_support::*;
use serde_json::{json, Value};

/// The member relaunch delay is 5 s; allow for detection and the fake's startup.
const RELAUNCH_TIMEOUT: Duration = Duration::from_secs(20);

fn reviewer() -> Value {
    json!({
        "name": "reviewer",
        "area": "Code review",
        "prompt": "Review strictly; never edit files.",
        "members": [
            {"name": "rc", "kind": "claude", "model": "rc"},
            {"name": "rx", "kind": "codex", "model": "rx"}
        ]
    })
}

/// A started server with the reviewer role defined and both members idle.
fn staffed(case: &str) -> (Orbit, Worker, Worker) {
    let mut orbit = Orbit::new(case);
    orbit.start();
    let defined = orbit.orbit_cli("team-define", &reviewer());
    assert!(defined.success, "{}", defined.raw);
    assert_eq!(
        defined.body["result"]["created_tab"], true,
        "{}",
        defined.raw
    );
    let rc = orbit.wait_worker("rc");
    let rx = orbit.wait_worker("rx");
    (orbit, rc, rx)
}

fn marker(task_id: &str) -> String {
    format!("[orbit-delivery {task_id} #1]")
}

#[test]
fn defined_role_owns_a_tab_after_polaris_and_takes_assignments_by_role() {
    let (orbit, rc, rx) = staffed("team-define");
    assert_eq!(orbit.polaris_tabs(), ["Polaris", "Reviewer", "Orbit"]);
    let role = orbit.team_role("reviewer");
    let members = role["members"].as_array().unwrap();
    assert_eq!(members.len(), 2, "{role}");
    assert_eq!(members[0]["pane_id"], json!(rc.pane_id));
    assert_eq!(members[1]["pane_id"], json!(rx.pane_id));
    assert!(
        members.iter().all(|member| member["state"] == "running"),
        "{role}"
    );
    assert_ne!(rc.pane_id, rx.pane_id);
    // Only the Claude member pre-allows its report command; never the lead.
    assert!(
        allows_only_report(&orbit.agent_argv("rc")),
        "{:?}",
        orbit.agent_argv("rc")
    );
    assert!(!orbit
        .agent_argv("rx")
        .iter()
        .any(|arg| arg.contains("allowed")));
    assert!(!orbit
        .agent_argv(LEAD)
        .iter()
        .any(|arg| arg.contains("allowed")));
    let inspect = orbit.result("orbit.inspect", json!({"role": "reviewer"}));
    assert_eq!(inspect["panes"].as_array().unwrap().len(), 2, "{inspect}");
    // Polaris kept focus.
    let lead = orbit.result("agent.get", json!({"target": "polaris"}))["agent"].clone();
    assert_eq!(lead["focused"], true, "{lead}");

    let response = orbit.request(
        "orbit.agent_assign",
        json!({"role": "reviewer", "kind": "codex", "prompt": "Review the parser"}),
    );
    let task_id = response["result"]["task_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{response}"))
        .to_owned();
    assert_eq!(response["result"]["worker_id"], json!(rx.terminal_id));
    let mut found = None;
    orbit.wait_until("assignment at the codex member", STATE_TIMEOUT, |orbit| {
        found = orbit.instruction("rx", &task_id);
        found.is_some()
    });
    let prompt = orbit
        .prompts("rx")
        .into_iter()
        .find(|prompt| prompt.contains(&task_id))
        .unwrap();
    assert!(prompt.contains("You act for the Reviewer role"), "{prompt}");
    let report = orbit.report(&found.unwrap(), "parser reviewed");
    assert!(report.success, "{}", report.raw);
    let delivered = orbit.wait_lead_prompt(&marker(&task_id));
    assert!(delivered.contains("parser reviewed"), "{delivered}");

    let refused = orbit.orbit_cli("agent-stop", &json!({"name": "rc"}));
    assert!(!refused.success, "{}", refused.raw);
    assert_eq!(
        refused.body["error"]["code"], "orbit_team_member_protected",
        "{}",
        refused.raw
    );
    assert!(orbit.agent_pid("rc").is_some());
    assert_eq!(orbit.wait_worker("rc").terminal_id, rc.terminal_id);
}

#[test]
fn killed_member_is_relaunched_in_its_own_pane() {
    let (orbit, rc, _) = staffed("team-relaunch");
    let pid = orbit.agent_pid("rc").unwrap();
    assert!(std::process::Command::new("kill")
        .arg(&pid)
        .status()
        .unwrap()
        .success());
    orbit.wait_until("rc relaunched", RELAUNCH_TIMEOUT, |orbit| {
        orbit.agent_pid("rc").is_some_and(|current| current != pid)
            && orbit.team_role("reviewer")["members"][0]["state"] == "running"
    });
    let relaunched = orbit.wait_worker("rc");
    assert_eq!(relaunched.pane_id, rc.pane_id);
    // The relaunch continues the session the member reported, with its role text.
    let screen = orbit.read_pane(&rc.pane_id);
    assert!(screen.contains("--resume rc"), "{screen}");
    assert!(screen.contains("--append-system-prompt"), "{screen}");
    assert_eq!(relaunched.terminal_id, rc.terminal_id);
    assert!(allows_only_report(&orbit.agent_argv("rc")));
    let role = orbit.team_role("reviewer");
    assert_eq!(role["members"][0]["name"], "rc");
    assert_eq!(role["members"][0]["terminal_id"], json!(rc.terminal_id));
    assert_eq!(orbit.polaris_tabs(), ["Polaris", "Reviewer", "Orbit"]);
}

#[test]
fn restart_restores_the_role_tab_and_resumed_members_report_to_the_lead() {
    let (mut orbit, rc, rx) = staffed("team-restart");
    let tab_id = orbit.team_role("reviewer")["tab_id"].clone();
    let response = orbit.request(
        "orbit.agent_assign",
        json!({"role": "reviewer", "kind": "claude", "prompt": "Survive a restart"}),
    );
    let task_id = response["result"]["task_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{response}"))
        .to_owned();
    let mut assignment = None;
    orbit.wait_until("assignment at the claude member", STATE_TIMEOUT, |orbit| {
        assignment = orbit.instruction("rc", &task_id);
        assignment.is_some()
    });
    orbit.stop();
    let roster = std::fs::read_to_string(orbit.team_path()).unwrap();
    assert!(
        roster.contains("\"version\": 1") && roster.contains("\"rc\""),
        "{roster}"
    );

    orbit.start();
    let resumed = orbit.wait_worker("rc");
    let codex = orbit.wait_worker("rx");
    assert_eq!(resumed.terminal_id, rc.terminal_id);
    assert_eq!(resumed.pane_id, rc.pane_id);
    assert_eq!(codex.terminal_id, rx.terminal_id);
    // Claude resumed natively; the restored plan kept its model and role text.
    let screen = orbit.read_pane(&resumed.pane_id);
    assert!(screen.contains("--resume rc"), "{screen}");
    assert!(screen.contains("--append-system-prompt"), "{screen}");
    let argv = orbit.agent_argv("rc");
    assert!(argv.iter().any(|arg| arg == "--resume"), "{argv:?}");
    assert!(allows_only_report(&argv), "{argv:?}");
    let screen = orbit.read_pane(&codex.pane_id);
    assert!(screen.contains("developer_instructions="), "{screen}");
    assert_eq!(orbit.polaris_tabs(), ["Polaris", "Reviewer", "Orbit"]);
    assert_eq!(orbit.team_role("reviewer")["tab_id"], tab_id);
    assert_eq!(orbit.delivery(&task_id), "awaiting_report");
    let report = orbit.report(&assignment.unwrap(), "reviewed after restart");
    assert!(report.success, "{}", report.raw);
    let delivered = orbit.wait_lead_prompt(&marker(&task_id));
    assert!(delivered.contains("reviewed after restart"), "{delivered}");
}
